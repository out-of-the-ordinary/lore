// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Refuses clients older than a configured fork build before any RPC runs.
//!
//! Clients announce themselves in `User-Agent` as `... lore/<version>+<build>`. With
//! `[server.client_policy] min_build` set, only a build named `<build_prefix><N>` with
//! `N >= min_build` is served; anyone else gets `FAILED_PRECONDITION` carrying the configured
//! update message. Mounted on the public gRPC server only.
use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;

use http::Request;
use http::Response;
use http::header::USER_AGENT;
use serde::Deserialize;
use tonic::Status;
use tower::Layer;
use tower::Service;
use tracing::info;

const DEFAULT_BUILD_PREFIX: &str = "ooto-main-";
const DEFAULT_UPDATE_MESSAGE: &str = "This server requires a newer Lore client.";
/// Bound on distinct refused user agents remembered for log de-duplication.
const MAX_LOGGED_AGENTS: usize = 256;

/// `[server.client_policy]`. Absent, or without `min_build`, the gate is off.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ClientPolicySettings {
    /// Lowest accepted build number.
    pub min_build: Option<u32>,
    /// A build name must be `<build_prefix><N>`.
    #[serde(default = "default_build_prefix")]
    pub build_prefix: String,
    /// Text returned to refused clients.
    #[serde(default = "default_update_message")]
    pub update_message: String,
}

fn default_build_prefix() -> String {
    DEFAULT_BUILD_PREFIX.to_owned()
}

fn default_update_message() -> String {
    DEFAULT_UPDATE_MESSAGE.to_owned()
}

/// The `lore/<version>` token of a `User-Agent`, if any.
#[lore_macro::test_pub]
fn lore_token(user_agent: &str) -> Option<&str> {
    user_agent
        .split_whitespace()
        .find(|token| token.starts_with("lore/") && token.len() > "lore/".len())
}

/// The build name of a `lore/<version>+<build>` token, if it carries one.
#[lore_macro::test_pub]
fn build_name(token: &str) -> Option<&str> {
    let (_, build) = token.strip_prefix("lore/")?.split_once('+')?;
    (!build.is_empty()).then_some(build)
}

/// The accept/refuse decision, built from [`ClientPolicySettings`].
#[derive(Debug)]
pub struct ClientPolicy {
    min_build: u32,
    build_prefix: String,
    update_message: String,
    logged: Mutex<HashSet<String>>,
}

impl ClientPolicy {
    /// `None` when the gate is off.
    pub fn from_settings(settings: &ClientPolicySettings) -> Option<Arc<Self>> {
        let min_build = settings.min_build?;
        Some(Arc::new(Self {
            min_build,
            build_prefix: settings.build_prefix.clone(),
            update_message: settings.update_message.clone(),
            logged: Mutex::new(HashSet::new()),
        }))
    }

    /// Whether a client with this `User-Agent` may be served.
    pub fn accepts(&self, user_agent: Option<&str>) -> bool {
        user_agent
            .and_then(lore_token)
            .and_then(build_name)
            .and_then(|build| build.strip_prefix(self.build_prefix.as_str()))
            .filter(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|number| number.parse::<u32>().ok())
            .is_some_and(|number| number >= self.min_build)
    }

    /// The refusal for this `User-Agent`, logging each distinct agent once.
    pub fn refusal(&self, user_agent: Option<&str>) -> Status {
        let seen = user_agent.and_then(lore_token).unwrap_or("unknown");
        let first = {
            let mut logged = self.logged.lock().unwrap_or_else(|e| e.into_inner());
            logged.len() < MAX_LOGGED_AGENTS && logged.insert(user_agent.unwrap_or("").to_owned())
        };
        if first {
            info!(
                user_agent = user_agent.unwrap_or("<none>"),
                "Refused a client below the minimum build"
            );
        }
        Status::failed_precondition(format!(
            "{} (client: {seen}, required: {}{} or newer)",
            self.update_message, self.build_prefix, self.min_build
        ))
    }
}

#[derive(Clone)]
pub struct ClientPolicyLayer {
    policy: Option<Arc<ClientPolicy>>,
}

impl ClientPolicyLayer {
    pub fn new(policy: Option<Arc<ClientPolicy>>) -> Self {
        Self { policy }
    }
}

impl<S> Layer<S> for ClientPolicyLayer {
    type Service = ClientPolicyService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ClientPolicyService {
            service: inner,
            policy: self.policy.clone(),
        }
    }
}

#[derive(Clone)]
pub struct ClientPolicyService<S> {
    service: S,
    policy: Option<Arc<ClientPolicy>>,
}

impl<S, B, C> Service<Request<B>> for ClientPolicyService<S>
where
    S: Service<Request<B>, Response = Response<C>>,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    C: Default + Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, request: Request<B>) -> Self::Future {
        if let Some(policy) = &self.policy {
            let user_agent = request
                .headers()
                .get(USER_AGENT)
                .and_then(|value| value.to_str().ok());
            if !policy.accepts(user_agent) {
                let response = policy.refusal(user_agent).into_http();
                return Box::pin(std::future::ready(Ok(response)));
            }
        }
        Box::pin(self.service.call(request))
    }
}
