// heeler-easytier: the config-server (EasyTier Web) session.
//
// Copyright (C) 2026 Heeler contributors
// SPDX-License-Identifier: LGPL-3.0-or-later
//
// `easytier-core --config-server` hands a config server EasyTier's whole
// process-management RPC. This session serves a narrower surface of its own
// (patches/0002-web-client-backend.patch makes that possible):
//
// - only WebClientService and ConfigRpc are registered: no peer, connector,
//   logger, credential, or port-forward management, and no file storage;
// - every network a server runs passes `outbound_only_config`, the policy a
//   local TOML gets, after `listener_urls` is dropped (EasyTier's console
//   adds listeners by default) and a credential file, disabled encryption or
//   managed credentials are refused;
// - one network at a time: another one fails and is reported to the server
//   as a failed instance;
// - config patches are refused (they would change a running network past the
//   policy), and the configuration the server sent is echoed back unchanged,
//   so its reconcile does not see the forced flags as drift and restart us;
// - network reports leave out every underlay address: this device's
//   interface, LAN and public addresses, and its peers' public addresses.

use std::{
    collections::{BTreeMap, HashMap},

    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use easytier::common::config::{ConfigLoader, TomlConfigLoader};
use easytier::instance::factory::create_native_instance;
use easytier::proto::{
    api::{
        instance::{PeerInfo, Route},
        config::{
            ConfigRpc, ConfigRpcServer, GetConfigRequest, GetConfigResponse, PatchConfigRequest,
            PatchConfigResponse,
        },
        manage::{
            CollectNetworkInfoRequest, CollectNetworkInfoResponse, ConfigSource,
            DeleteNetworkInstanceRequest, DeleteNetworkInstanceResponse,
            GetNetworkInstanceConfigRequest, GetNetworkInstanceConfigResponse,
            ListNetworkInstanceMetaRequest, ListNetworkInstanceMetaResponse,
            ListNetworkInstanceRequest, ListNetworkInstanceResponse, NetworkConfig,
            NetworkInstanceRunningInfo, NetworkInstanceRunningInfoMap, NetworkMeta, RetainNetworkInstanceRequest,
            RetainNetworkInstanceResponse, RunNetworkInstanceRequest, RunNetworkInstanceResponse,
            ValidateConfigRequest, ValidateConfigResponse, WebClientService,
            WebClientServiceServer,
        },
    },
    common::StunInfo,
    rpc_types::{self, controller::BaseController},
};
use easytier::web_client::{parse_config_server_endpoint, run_web_client_with_backend};
use easytier_core::{
    config::api_input::NetworkConfigExt as _,
    management::{WebClient, WebClientBackend, network_instance_running_info},
    rpc::service_registry::ServiceRegistry,
};
use tokio::runtime::Runtime;
use uuid::Uuid;

use crate::{CURRENT, Failure, Owner, Running, lock, outbound_only_config, stop_bounded};

/// How long a server-sent network may take to start.
const WEB_START_TIMEOUT: Duration = Duration::from_secs(30);
/// The name the server lists this device under when the app gives none.
const FALLBACK_HOSTNAME: &str = "Heeler";
/// How many refused networks the session remembers (the most recent ones), so
/// a server cannot grow the state and the status without bound.
const MAX_FAILURES: usize = 16;

/// The running session, if any.
static SESSION: Mutex<Option<Session>> = Mutex::new(None);

struct Session {
    key: SessionKey,
    client: WebClient<()>,
    state: Arc<WebState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionKey {
    url: String,
    machine_id: Uuid,
    hostname: String,
    secure_mode: bool,
}

/// A network the server asked for and this session refused or failed to run.
#[derive(Debug, Clone)]
struct RunFailure {
    network_name: String,
    message: String,
    /// Increases with every recorded failure; the oldest is dropped first.
    sequence: u64,
}

struct WebState {
    machine_id: Uuid,
    hostname: String,
    /// The network this session runs: its instance ID and the configuration
    /// exactly as the server sent it.
    owned: Mutex<Option<(Uuid, NetworkConfig)>>,
    failures: Mutex<HashMap<Uuid, RunFailure>>,
    failure_sequence: AtomicU64,
    generation: AtomicUsize,
    changed: tokio::sync::Notify,
    /// Serializes run, retain and delete with each other and with shutdown.
    mutation: tokio::sync::Mutex<()>,
    /// Set by shutdown: no handler may start a network afterwards.
    closed: AtomicBool,
}

impl WebState {
    fn new(machine_id: Uuid, hostname: String) -> Self {
        Self {
            machine_id,
            hostname,
            owned: Mutex::new(None),
            failures: Mutex::new(HashMap::new()),
            failure_sequence: AtomicU64::new(0),
            generation: AtomicUsize::new(0),
            changed: tokio::sync::Notify::new(),
            mutation: tokio::sync::Mutex::new(()),
            closed: AtomicBool::new(false),
        }
    }

    fn owned_id(&self) -> Option<Uuid> {
        lock(&self.owned).as_ref().map(|(id, _)| *id)
    }

    fn owned_config(&self, id: Uuid) -> Option<NetworkConfig> {
        lock(&self.owned)
            .as_ref()
            .filter(|(owned, _)| *owned == id)
            .map(|(_, config)| config.clone())
    }

    /// Records a refused network, keeping the `MAX_FAILURES` most recent.
    fn record_failure(&self, id: Uuid, network_name: String, message: String) {
        let sequence = self.failure_sequence.fetch_add(1, Ordering::Relaxed);
        let mut failures = lock(&self.failures);
        failures.insert(id, RunFailure { network_name, message, sequence });
        while failures.len() > MAX_FAILURES {
            let Some(oldest) = failures.iter().min_by_key(|(_, failure)| failure.sequence).map(|(id, _)| *id)
            else {
                break;
            };
            failures.remove(&oldest);
        }
    }

    /// Wakes the heartbeat so the server hears about a change at once.
    fn changed(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.changed.notify_waiters();
    }
}

fn rpc_error(message: impl Into<String>) -> rpc_types::error::Error {
    anyhow::anyhow!(message.into()).into()
}

/// The network a server-sent configuration becomes, under the outbound-only
/// policy: listeners dropped, `hostname` filled in when the server sent none,
/// and the instance ID set to `id`. Returns the checked loader and its TOML.
fn checked_web_config(
    config: &NetworkConfig,
    id: Uuid,
    hostname: &str,
) -> Result<(TomlConfigLoader, String), String> {
    let refuse = |what: &str| Err(format!("{what} is not supported: Heeler only dials out"));
    if config.credential_file.as_deref().is_some_and(|file| !file.trim().is_empty()) {
        return refuse("credential_file");
    }
    if config.disable_encryption == Some(true) {
        return Err("disable_encryption is not supported: Heeler keeps traffic encrypted".to_owned());
    }
    if !config.managed_credentials.is_empty() {
        return refuse("managed_credentials");
    }
    let mut config = config.clone();
    config.instance_id = Some(id.to_string());
    // Listening is never allowed, and EasyTier's console adds tcp, udp and
    // wg listeners to every new network; dropping them only narrows it.
    config.listener_urls.clear();
    if config.hostname.as_deref().is_none_or(|name| name.trim().is_empty()) {
        config.hostname = Some(hostname.to_owned());
    }
    let loader = config.gen_config().map_err(|error| format!("{error:#}"))?;
    let checked = outbound_only_config(&loader.dump()).map_err(|failure| failure.message)?;
    let toml = checked.dump();
    Ok((checked, toml))
}

/// Stops the session's network, if it still owns the running one.
async fn stop_owned(state: &WebState) {
    let previous = {
        let mut current = lock(&CURRENT);
        if current.as_ref().is_some_and(|running| running.owner == Owner::Web) {
            current.take()
        } else {
            None
        }
    };
    if let Some(previous) = previous {
        stop_bounded(&previous.instance).await;
    }
    *lock(&state.owned) = None;
}

fn owned_instance(state: &WebState) -> Option<(Uuid, Arc<crate::NativeCoreInstance>)> {
    let id = state.owned_id()?;
    let current = lock(&CURRENT);
    current
        .as_ref()
        .filter(|running| running.owner == Owner::Web)
        .map(|running| (id, running.instance.clone()))
}

/// Strips every underlay address from a network report: the server learns
/// the virtual network (addresses, hostnames, routes, costs, latencies, NAT
/// types, tunnel types), not this device's interface, LAN or public
/// addresses, its ports, or the public addresses of its peers.
fn redact_running_info(info: &mut NetworkInstanceRunningInfo) {
    fn redact_stun(stun: &mut Option<StunInfo>) {
        if let Some(stun) = stun.as_mut() {
            stun.public_ip.clear();
            stun.min_port = 0;
            stun.max_port = 0;
        }
    }
    fn redact_route(route: &mut Route) {
        redact_stun(&mut route.stun_info);
        route.public_ipv6_addr = None;
        route.ipv6_public_addr_prefix = None;
    }
    fn redact_peer(peer: &mut PeerInfo) {
        for conn in &mut peer.conns {
            if let Some(tunnel) = conn.tunnel.as_mut() {
                tunnel.local_addr = None;
                tunnel.remote_addr = None;
                tunnel.resolved_remote_addr = None;
            }
        }
    }
    if let Some(node) = info.my_node_info.as_mut() {
        node.ips = None;
        node.listeners.clear();
        redact_stun(&mut node.stun_info);
    }
    // Management events name tunnels and listeners by address.
    info.events.clear();
    info.routes.iter_mut().for_each(redact_route);
    info.peers.iter_mut().for_each(redact_peer);
    for pair in &mut info.peer_route_pairs {
        if let Some(route) = pair.route.as_mut() {
            redact_route(route);
        }
        if let Some(peer) = pair.peer.as_mut() {
            redact_peer(peer);
        }
    }
}

#[derive(Clone)]
struct Backend(Arc<WebState>);

impl Backend {
    async fn run(&self, request: RunNetworkInstanceRequest) -> Result<Uuid, (Uuid, String, String)> {
        let state = &self.0;
        let original = request.config.unwrap_or_default();
        let network_name = original.network_name.clone().unwrap_or_default();
        let id = request
            .inst_id
            .map(Uuid::from)
            .or_else(|| original.instance_id.as_deref().and_then(|raw| Uuid::parse_str(raw).ok()))
            .unwrap_or_else(Uuid::new_v4);
        let fail = |message: String| (id, network_name.clone(), message);

        if state.closed.load(Ordering::Acquire) {
            return Err(fail("the config-server session has ended".to_owned()));
        }
        if let Some(owned) = state.owned_id() {
            if owned != id {
                let running = lock(&state.owned)
                    .as_ref()
                    .and_then(|(_, config)| config.network_name.clone())
                    .unwrap_or_default();
                return Err(fail(format!(
                    "Heeler runs one EasyTier network per device and \"{running}\" is already running; \
                     disable or delete it first"
                )));
            }
            let live = owned_instance(state).is_some_and(|(_, instance)| crate::is_live(&instance));
            if live && !request.overwrite {
                return Ok(id);
            }
        }
        let (checked, toml) = checked_web_config(&original, id, &state.hostname).map_err(fail)?;
        stop_owned(state).await;
        let instance = create_native_instance(checked).map_err(|error| fail(format!("{error:#}")))?;
        match tokio::time::timeout(WEB_START_TIMEOUT, instance.start()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                stop_bounded(&instance).await;
                return Err(fail(format!("{error:#}")));
            }
            Err(_) => {
                stop_bounded(&instance).await;
                return Err(fail(format!("EasyTier did not start within {WEB_START_TIMEOUT:?}")));
            }
        }
        *lock(&CURRENT) = Some(Running { instance, toml, owner: Owner::Web });
        *lock(&state.owned) = Some((id, original));
        Ok(id)
    }

    /// Stops the network unless `keep` holds its ID, and forgets failures
    /// `forget` matches.
    async fn remove(&self, keep: impl Fn(&Uuid) -> bool) -> Vec<Uuid> {
        let state = &self.0;
        lock(&state.failures).retain(|id, _| keep(id));
        if state.owned_id().is_some_and(|owned| !keep(&owned)) {
            stop_owned(state).await;
        }
        state.changed();
        state.owned_id().into_iter().collect()
    }
}

#[async_trait]
impl WebClientBackend for Backend {
    fn register(&self, registry: &ServiceRegistry) {
        registry.register(WebClientServiceServer::new(self.clone()), "");
        registry.register(ConfigRpcServer::new(self.clone()), "");
    }

    async fn instance_ids(&self) -> anyhow::Result<Vec<Uuid>> {
        Ok(self.0.owned_id().into_iter().collect())
    }

    fn failed_instance_ids(&self) -> Vec<Uuid> {
        let mut ids = lock(&self.0.failures).keys().copied().collect::<Vec<_>>();
        ids.sort_unstable();
        ids
    }

    fn instance_state_generation(&self) -> usize {
        self.0.generation.load(Ordering::Acquire)
    }

    async fn wait_for_instance_state_change(&self, generation: usize) -> usize {
        loop {
            let notified = self.0.changed.notified();
            let now = self.0.generation.load(Ordering::Acquire);
            if now != generation {
                return now;
            }
            notified.await;
        }
    }
}

#[async_trait]
impl WebClientService for Backend {
    type Controller = BaseController;

    async fn validate_config(
        &self,
        _: BaseController,
        request: ValidateConfigRequest,
    ) -> rpc_types::error::Result<ValidateConfigResponse> {
        let config = request.config.unwrap_or_default();
        let id = config
            .instance_id
            .as_deref()
            .and_then(|raw| Uuid::parse_str(raw).ok())
            .unwrap_or_else(Uuid::new_v4);
        let (_, toml_config) = checked_web_config(&config, id, &self.0.hostname).map_err(rpc_error)?;
        Ok(ValidateConfigResponse { toml_config })
    }

    async fn run_network_instance(
        &self,
        _: BaseController,
        request: RunNetworkInstanceRequest,
    ) -> rpc_types::error::Result<RunNetworkInstanceResponse> {
        let state = &self.0;
        let _mutation = state.mutation.lock().await;
        let result = self.run(request).await;
        match &result {
            Ok(id) => {
                lock(&state.failures).remove(id);
            }
            Err((id, network_name, message)) => {
                state.record_failure(*id, network_name.clone(), message.clone());
            }
        }
        state.changed();
        let id = result.map_err(|(_, _, message)| rpc_error(message))?;
        Ok(RunNetworkInstanceResponse { inst_id: Some(id.into()) })
    }

    async fn retain_network_instance(
        &self,
        _: BaseController,
        request: RetainNetworkInstanceRequest,
    ) -> rpc_types::error::Result<RetainNetworkInstanceResponse> {
        let _mutation = self.0.mutation.lock().await;
        let keep = request.inst_ids.into_iter().map(Uuid::from).collect::<Vec<_>>();
        let remaining = self.remove(|id| keep.contains(id)).await;
        Ok(RetainNetworkInstanceResponse {
            remain_inst_ids: remaining.into_iter().map(Into::into).collect(),
        })
    }

    async fn collect_network_info(
        &self,
        _: BaseController,
        request: CollectNetworkInfoRequest,
    ) -> rpc_types::error::Result<CollectNetworkInfoResponse> {
        let wanted = request.inst_ids.into_iter().map(Uuid::from).collect::<Vec<_>>();
        let mut map = BTreeMap::new();
        if let Some((id, instance)) = owned_instance(&self.0)
            && (wanted.is_empty() || wanted.contains(&id))
        {
            let mut info = network_instance_running_info(&instance).await?;
            redact_running_info(&mut info);
            map.insert(id.to_string(), info);
        }
        Ok(CollectNetworkInfoResponse {
            info: Some(NetworkInstanceRunningInfoMap { map: map.into_iter().collect() }),
        })
    }

    async fn list_network_instance(
        &self,
        _: BaseController,
        _: ListNetworkInstanceRequest,
    ) -> rpc_types::error::Result<ListNetworkInstanceResponse> {
        Ok(ListNetworkInstanceResponse {
            inst_ids: self.0.owned_id().into_iter().map(Into::into).collect(),
        })
    }

    async fn delete_network_instance(
        &self,
        _: BaseController,
        request: DeleteNetworkInstanceRequest,
    ) -> rpc_types::error::Result<DeleteNetworkInstanceResponse> {
        let _mutation = self.0.mutation.lock().await;
        let gone = request.inst_ids.into_iter().map(Uuid::from).collect::<Vec<_>>();
        let remaining = self.remove(|id| !gone.contains(id)).await;
        Ok(DeleteNetworkInstanceResponse {
            remain_inst_ids: remaining.into_iter().map(Into::into).collect(),
        })
    }

    async fn get_network_instance_config(
        &self,
        _: BaseController,
        request: GetNetworkInstanceConfigRequest,
    ) -> rpc_types::error::Result<GetNetworkInstanceConfigResponse> {
        let id = Uuid::from(request.inst_id.ok_or_else(|| rpc_error("instance id is required"))?);
        let config = self
            .0
            .owned_config(id)
            .ok_or_else(|| rpc_error("instance config control not found"))?;
        Ok(GetNetworkInstanceConfigResponse {
            config: Some(config),
            source: ConfigSource::Web as i32,
        })
    }

    async fn list_network_instance_meta(
        &self,
        _: BaseController,
        request: ListNetworkInstanceMetaRequest,
    ) -> rpc_types::error::Result<ListNetworkInstanceMetaResponse> {
        let metas = request
            .inst_ids
            .into_iter()
            .map(Uuid::from)
            .filter_map(|id| self.0.owned_config(id).map(|config| (id, config)))
            .map(|(id, config)| {
                let name = config.network_name.unwrap_or_default();
                NetworkMeta {
                    inst_id: Some(id.into()),
                    network_name: name.clone(),
                    config_permission: 0,
                    instance_name: name,
                    source: ConfigSource::Web as i32,
                }
            })
            .collect();
        Ok(ListNetworkInstanceMetaResponse { metas })
    }
}

#[async_trait]
impl ConfigRpc for Backend {
    type Controller = BaseController;

    async fn patch_config(
        &self,
        _: BaseController,
        _: PatchConfigRequest,
    ) -> rpc_types::error::Result<PatchConfigResponse> {
        Err(rpc_error(
            "Heeler does not apply config patches; save the network to run it again",
        ))
    }

    async fn get_config(
        &self,
        _: BaseController,
        _: GetConfigRequest,
    ) -> rpc_types::error::Result<GetConfigResponse> {
        let config = lock(&self.0.owned)
            .as_ref()
            .map(|(_, config)| config.clone())
            .ok_or_else(|| rpc_error("no network is running"))?;
        let toml_config = lock(&CURRENT)
            .as_ref()
            .filter(|running| running.owner == Owner::Web)
            .map(|running| running.toml.clone())
            .unwrap_or_default();
        Ok(GetConfigResponse { config: Some(config), toml_config })
    }
}

/// Checks a config-server URL: `udp://` or `tcp://` with a host and a port, or
/// `ws://` or `wss://` with a host (the port defaults to 80 or 443), and a
/// non-empty token as the last path segment. wss:// certificates are verified
/// against the system trust store when the session connects.
fn check_url(url: &str) -> Result<(), Failure> {
    let endpoint = parse_config_server_endpoint(url).map_err(Failure::from)?;
    let connect = endpoint.connect_url();
    let port = match connect.scheme() {
        "udp" | "tcp" => connect.port(),
        "ws" | "wss" => connect.port_or_known_default(),
        scheme => {
            return Err(Failure::generic(format!(
                "config server URLs use udp://, tcp://, ws:// or wss://, not {scheme}://"
            )));
        }
    };
    if connect.host_str().is_none_or(str::is_empty) || port.is_none() {
        return Err(Failure::generic("the config server URL needs a host and a port"));
    }
    Ok(())
}

fn parse_machine_id(text: &str) -> Result<Uuid, Failure> {
    match Uuid::parse_str(text.trim()) {
        Ok(id) if !id.is_nil() => Ok(id),
        _ => Err(Failure::generic("the machine ID is not a UUID")),
    }
}

/// Ends the session and stops its network. The caller holds LIFECYCLE.
pub(crate) fn shutdown(rt: &Runtime) {
    let Some(session) = lock(&SESSION).take() else { return };
    let state = session.state.clone();
    state.closed.store(true, Ordering::Release);
    rt.block_on(async {
        // Waits out a handler that is starting or stopping a network.
        let _mutation = state.mutation.lock().await;
        stop_owned(&state).await;
    });
    let _context = rt.enter();
    drop(session);
}

/// Starts (or keeps) the session for `url`, replacing any other session or
/// manual network.
pub(crate) fn start(
    url: &str,
    machine_id: &str,
    hostname: &str,
    secure_mode: bool,
) -> Result<std::ffi::c_int, Failure> {
    let rt = crate::runtime()?;
    check_url(url)?;
    let machine_id = parse_machine_id(machine_id)?;
    let hostname = match hostname.trim() {
        "" => FALLBACK_HOSTNAME.to_owned(),
        name => name.to_owned(),
    };
    let key = SessionKey { url: url.to_owned(), machine_id, hostname: hostname.clone(), secure_mode };
    let _lifecycle = lock(&crate::LIFECYCLE);
    if lock(&SESSION).as_ref().is_some_and(|session| session.key == key) {
        return Ok(crate::HEELER_ET_OK);
    }
    shutdown(rt);
    let previous = lock(&CURRENT).take();
    if let Some(previous) = previous {
        rt.block_on(async move { stop_bounded(&previous.instance).await });
    }
    let state = Arc::new(WebState::new(machine_id, hostname.clone()));
    let _context = rt.enter();
    // Either way the session upgrades to EasyTier's encrypted (Noise NN) web
    // tunnel whenever the server offers it. With secure_mode a server that
    // does not offer it — or a path that strips the offer from the plaintext
    // feature probe — is retried, never used in clear text, so the token and
    // network secrets are not sent unencrypted (a ws:// URL still carries the
    // token in its HTTP upgrade request). Without it such a server is used in
    // clear text: the token and the networks it sends can be read and changed
    // on the path. Noise NN does not authenticate the server; only wss://
    // (patch 0003) does.
    let client =
        run_web_client_with_backend(url, machine_id, hostname, secure_mode, Arc::new(Backend(state.clone())))?;
    *lock(&SESSION) = Some(Session { key, client, state });
    Ok(crate::HEELER_ET_OK)
}

/// Ends the session and its network, if any.
pub(crate) fn stop() {
    let Ok(rt) = crate::runtime() else { return };
    let _lifecycle = lock(&crate::LIFECYCLE);
    shutdown(rt);
}

/// The session's `web` status object, or nil without a session.
pub(crate) fn status() -> Option<serde_json::Value> {
    let session = lock(&SESSION);
    let session = session.as_ref()?;
    let state = &session.state;
    let owned = lock(&state.owned).clone();
    let mut failures = lock(&state.failures)
        .iter()
        .map(|(id, failure)| (*id, failure.clone()))
        .collect::<Vec<_>>();
    failures.sort_by_key(|(_, failure)| failure.sequence);
    Some(serde_json::json!({
        "connected": session.client.is_connected(),
        "machine_id": state.machine_id.to_string(),
        "instance_id": owned.as_ref().map(|(id, _)| id.to_string()),
        "network_name": owned.and_then(|(_, config)| config.network_name),
        "failures": failures
            .into_iter()
            .map(|(id, failure)| serde_json::json!({
                "instance_id": id.to_string(),
                "network_name": failure.network_name,
                "message": failure.message,
            }))
            .collect::<Vec<_>>(),
    }))
}


#[cfg(test)]
mod tests {
    use super::*;

    fn network() -> NetworkConfig {
        NetworkConfig {
            network_name: Some("home".to_owned()),
            network_secret: Some("s3cret".to_owned()),
            networking_method: Some(1),
            peer_urls: vec!["tcp://127.0.0.1:11010".to_owned()],
            dhcp: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn server_networks_lose_their_listeners_and_get_the_policy_flags() {
        let id = Uuid::new_v4();
        let mut config = network();
        config.listener_urls =
            vec!["tcp://0.0.0.0:11010".to_owned(), "udp://0.0.0.0:11010".to_owned(), "wg://0.0.0.0:11011".to_owned()];
        config.no_tun = Some(false);
        config.enable_exit_node = Some(true);
        config.enable_private_mode = Some(false);
        let (checked, toml) = checked_web_config(&config, id, "phone").expect("accepted");
        assert!(checked.get_listeners().is_none_or(|listeners| listeners.is_empty()));
        let flags = checked.get_flags();
        assert!(flags.no_tun && !flags.enable_exit_node && flags.private_mode && flags.disable_relay_data);
        assert_eq!(flags.relay_network_whitelist, "");
        assert_eq!(checked.get_id(), id);
        assert_eq!(checked.get_hostname(), "phone");
        assert!(toml.contains("no_tun = true"));
    }

    #[test]
    fn a_server_hostname_wins_over_the_apps() {
        let mut config = network();
        config.hostname = Some("from-console".to_owned());
        let (checked, _) = checked_web_config(&config, Uuid::new_v4(), "phone").expect("accepted");
        assert_eq!(checked.get_hostname(), "from-console");
    }

    #[test]
    fn inbound_and_unsafe_server_networks_are_refused() {
        let cases: Vec<(&str, Box<dyn Fn(&mut NetworkConfig)>)> = vec![
            ("proxy_network", Box::new(|c| c.proxy_cidrs = vec!["192.168.1.0/24".to_owned()])),
            ("exit_nodes", Box::new(|c| c.exit_nodes = vec!["10.0.0.1".to_owned()])),
            ("socks5", Box::new(|c| {
                c.enable_socks5 = Some(true);
                c.socks5_port = Some(1080);
            })),
            ("mapped listeners", Box::new(|c| c.mapped_listeners = vec!["tcp://1.2.3.4:11010".to_owned()])),
            ("credential_file", Box::new(|c| c.credential_file = Some("/etc/passwd".to_owned()))),
            ("disable_encryption", Box::new(|c| c.disable_encryption = Some(true))),
            ("empty secret", Box::new(|c| c.network_secret = Some(String::new()))),
        ];
        for (what, mutate) in cases {
            let mut config = network();
            mutate(&mut config);
            assert!(checked_web_config(&config, Uuid::new_v4(), "phone").is_err(), "accepted {what}");
        }
    }

    #[test]
    fn config_server_urls_need_a_known_transport_host_port_and_token() {
        assert!(check_url("udp://config-server.easytier.cn:22020/alice").is_ok());
        assert!(check_url("tcp://10.0.0.1:22020/team%2Fbob").is_ok());
        assert!(check_url("ws://example.com:8080/alice").is_ok());
        assert!(check_url("wss://example.com/api/alice").is_ok());
        for url in [
            "alice",
            "udp://config-server.easytier.cn:22020",
            "udp://config-server.easytier.cn:22020/",
            "udp://config-server.easytier.cn/alice",
            "wss://example.com/",
            "ring://x/alice",
            "unix:///tmp/socket/alice",
            "http://example.com:80/alice",
        ] {
            assert!(check_url(url).is_err(), "accepted {url}");
        }
        assert!(parse_machine_id("6a1f0e44-6c1e-4f43-9b7f-1f1f1f1f1f1f").is_ok());
        assert!(parse_machine_id("00000000-0000-0000-0000-000000000000").is_err());
        assert!(parse_machine_id("phone").is_err());
    }

    #[test]
    fn network_reports_carry_no_underlay_addresses() {
        use easytier::proto::{
            api::{
                instance::{PeerConnInfo, PeerRoutePair},
                manage::MyNodeInfo,
            },
            common::{Ipv4Addr as PbIpv4, Ipv6Addr as PbIpv6, Ipv6Inet, TunnelInfo, Url},
            peer_rpc::GetIpListResponse,
        };
        let url = |text: &str| Some(Url { url: text.to_owned() });
        let stun = || {
            Some(StunInfo {
                udp_nat_type: 3,
                public_ip: vec!["203.0.113.7".to_owned()],
                min_port: 40000,
                max_port: 40100,
                ..Default::default()
            })
        };
        let public_v6 = || {
            Some(Ipv6Inet { address: Some(PbIpv6 { part1: 0x2001_0db8, ..Default::default() }), network_length: 64 })
        };
        let route = || Route {
            peer_id: 7,
            hostname: "peer".to_owned(),
            cost: 1,
            stun_info: stun(),
            public_ipv6_addr: public_v6(),
            ipv6_public_addr_prefix: public_v6(),
            ..Default::default()
        };
        let peer = || PeerInfo {
            peer_id: 7,
            conns: vec![PeerConnInfo {
                conn_id: "c".to_owned(),
                tunnel: Some(TunnelInfo {
                    tunnel_type: "udp".to_owned(),
                    local_addr: url("udp://192.168.1.20:51000"),
                    remote_addr: url("udp://198.51.100.9:11010"),
                    resolved_remote_addr: url("udp://198.51.100.9:11010"),
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut info = NetworkInstanceRunningInfo {
            my_node_info: Some(MyNodeInfo {
                hostname: "phone".to_owned(),
                ips: Some(GetIpListResponse {
                    public_ipv4: Some(PbIpv4 { addr: 0xcb00_7107 }),
                    interface_ipv4s: vec![PbIpv4 { addr: 0xc0a8_0114 }],
                    ..Default::default()
                }),
                stun_info: stun(),
                listeners: vec![Url { url: "udp://192.168.1.20:11010".to_owned() }],
                peer_id: 1,
                ..Default::default()
            }),
            events: vec!["connected to udp://198.51.100.9:11010 from 192.168.1.20".to_owned()],
            routes: vec![route()],
            peers: vec![peer()],
            peer_route_pairs: vec![PeerRoutePair { route: Some(route()), peer: Some(peer()) }],
            running: true,
            ..Default::default()
        };
        redact_running_info(&mut info);

        let node = info.my_node_info.as_ref().expect("node");
        assert!(node.ips.is_none() && node.listeners.is_empty());
        assert_eq!(node.hostname, "phone");
        let node_stun = node.stun_info.as_ref().expect("stun");
        assert!(node_stun.public_ip.is_empty() && node_stun.min_port == 0 && node_stun.max_port == 0);
        assert_eq!(node_stun.udp_nat_type, 3, "the NAT type stays");
        assert!(info.events.is_empty());
        let routes = info.routes.iter().chain(info.peer_route_pairs.iter().filter_map(|pair| pair.route.as_ref()));
        for route in routes {
            assert!(route.public_ipv6_addr.is_none() && route.ipv6_public_addr_prefix.is_none());
            assert!(route.stun_info.as_ref().is_some_and(|stun| stun.public_ip.is_empty()));
            assert_eq!((route.peer_id, route.hostname.as_str(), route.cost), (7, "peer", 1));
        }
        let peers = info.peers.iter().chain(info.peer_route_pairs.iter().filter_map(|pair| pair.peer.as_ref()));
        for peer in peers {
            let tunnel = peer.conns[0].tunnel.as_ref().expect("tunnel");
            assert_eq!(tunnel.tunnel_type, "udp");
            assert!(tunnel.local_addr.is_none() && tunnel.remote_addr.is_none() && tunnel.resolved_remote_addr.is_none());
        }
        // Nothing address-like survives anywhere in the report.
        let text = format!("{info:?}");
        for leaked in ["192.168", "198.51.100", "203.0.113", "51000", "40000"] {
            assert!(!text.contains(leaked), "{leaked} in {text}");
        }
    }

    #[test]
    fn only_the_most_recent_failures_are_kept() {
        let state = WebState::new(Uuid::new_v4(), "phone".to_owned());
        let ids = (0..MAX_FAILURES + 4).map(|_| Uuid::new_v4()).collect::<Vec<_>>();
        for (index, id) in ids.iter().enumerate() {
            state.record_failure(*id, format!("net-{index}"), "refused".to_owned());
        }
        let failures = lock(&state.failures);
        assert_eq!(failures.len(), MAX_FAILURES);
        assert!(ids[..4].iter().all(|id| !failures.contains_key(id)));
        assert!(ids[4..].iter().all(|id| failures.contains_key(id)));
        drop(failures);
        // A refused network that is refused again is not counted twice.
        state.record_failure(ids[10], "again".to_owned(), "refused".to_owned());
        assert_eq!(lock(&state.failures).len(), MAX_FAILURES);
        assert!(lock(&state.failures).contains_key(&ids[4]));
    }

    #[tokio::test]
    async fn patches_are_refused_and_unknown_networks_have_no_config() {
        let backend = Backend(Arc::new(WebState::new(Uuid::new_v4(), "phone".to_owned())));
        let patched = ConfigRpc::patch_config(&backend, BaseController::default(), PatchConfigRequest::default()).await;
        assert!(patched.is_err());
        let config = backend
            .get_network_instance_config(
                BaseController::default(),
                GetNetworkInstanceConfigRequest { inst_id: Some(Uuid::new_v4().into()) },
            )
            .await;
        assert!(config.is_err());
        let listed = backend
            .list_network_instance(BaseController::default(), ListNetworkInstanceRequest {})
            .await
            .expect("listed");
        assert!(listed.inst_ids.is_empty());
    }

    #[tokio::test]
    async fn a_second_network_is_refused_and_reported_as_failed() {
        let state = Arc::new(WebState::new(Uuid::new_v4(), "phone".to_owned()));
        let running = Uuid::new_v4();
        *lock(&state.owned) = Some((running, network()));
        let backend = Backend(state.clone());
        let other = Uuid::new_v4();
        let mut config = network();
        config.network_name = Some("other".to_owned());
        let result = backend
            .run_network_instance(
                BaseController::default(),
                RunNetworkInstanceRequest {
                    inst_id: Some(other.into()),
                    config: Some(config),
                    overwrite: false,
                    source: ConfigSource::Web as i32,
                },
            )
            .await;
        assert!(result.is_err());
        assert_eq!(backend.failed_instance_ids(), vec![other]);
        assert_eq!(lock(&state.failures)[&other].network_name, "other");
        // The running network's configuration is echoed back verbatim.
        let echoed = backend
            .get_network_instance_config(
                BaseController::default(),
                GetNetworkInstanceConfigRequest { inst_id: Some(running.into()) },
            )
            .await
            .expect("echoed");
        assert_eq!(echoed.config, Some(network()));
        // Deleting the refused network clears its failure.
        backend
            .delete_network_instance(
                BaseController::default(),
                DeleteNetworkInstanceRequest { inst_ids: vec![other.into()] },
            )
            .await
            .expect("deleted");
        assert!(backend.failed_instance_ids().is_empty());
    }
}
