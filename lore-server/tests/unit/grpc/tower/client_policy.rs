// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::convert::Infallible;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use http::Request;
use http::Response;
use http::header::USER_AGENT;
use lore_server::grpc::tower::client_policy::*;
use tonic::Code;
use tonic::Status;
use tower::Layer;
use tower::Service;

fn policy(min_build: Option<u32>) -> Option<Arc<ClientPolicy>> {
    let settings: ClientPolicySettings = toml::from_str(&match min_build {
        Some(n) => {
            format!("min_build = {n}\nupdate_message = \"Update: https://example.test/lore\"")
        }
        None => String::new(),
    })
    .expect("settings parse");
    ClientPolicy::from_settings(&settings)
}

mod parsing {
    use super::*;

    #[test]
    fn finds_the_lore_token_among_others() {
        assert_eq!(
            lore_token("app product lore/0.14.1-nightly+ooto-main-5 tonic/0.14"),
            Some("lore/0.14.1-nightly+ooto-main-5")
        );
    }

    #[test]
    fn missing_or_garbled_token_is_none() {
        assert_eq!(lore_token(""), None);
        assert_eq!(lore_token("curl/8.0"), None);
        assert_eq!(lore_token("lore/"), None);
        assert_eq!(lore_token("notlore/1.0+ooto-main-5"), None);
    }

    #[test]
    fn build_name_follows_the_plus() {
        assert_eq!(build_name("lore/0.14.1+ooto-main-5"), Some("ooto-main-5"));
        assert_eq!(build_name("lore/0.14.1+local"), Some("local"));
        assert_eq!(build_name("lore/0.14.1"), None);
        assert_eq!(build_name("lore/0.14.1+"), None);
    }
}

mod accepts {
    use super::*;

    #[test]
    fn matrix() {
        let p = policy(Some(5)).unwrap();
        let ua = |b: &str| format!("lore/0.14.1+{b} tonic/0.14");
        assert!(p.accepts(Some(&ua("ooto-main-5"))));
        assert!(p.accepts(Some(&ua("ooto-main-12"))));
        assert!(!p.accepts(Some(&ua("ooto-main-4"))));
        assert!(!p.accepts(Some(&ua("ooto-main-"))));
        assert!(!p.accepts(Some(&ua("ooto-main-5x"))));
        assert!(!p.accepts(Some(&ua("ooto-main-99999999999"))));
        assert!(!p.accepts(Some(&ua("local"))));
        assert!(!p.accepts(Some(&ua("upstream-9"))));
        assert!(!p.accepts(Some("lore/0.14.1")));
        assert!(!p.accepts(Some("curl/8.0")));
        assert!(!p.accepts(None));
    }

    #[test]
    fn custom_prefix() {
        let settings: ClientPolicySettings =
            toml::from_str("min_build = 2\nbuild_prefix = \"acme-\"").unwrap();
        let p = ClientPolicy::from_settings(&settings).unwrap();
        assert!(p.accepts(Some("lore/1+acme-2")));
        assert!(!p.accepts(Some("lore/1+ooto-main-9")));
    }

    #[test]
    fn feature_is_off_without_min_build() {
        assert!(policy(None).is_none());
    }

    #[test]
    fn refusal_message_names_client_and_requirement() {
        let p = policy(Some(5)).unwrap();
        let status = p.refusal(Some("lore/0.13.1+ooto-main-2"));
        assert_eq!(status.code(), Code::FailedPrecondition);
        assert_eq!(
            status.message(),
            "Update: https://example.test/lore (client: lore/0.13.1+ooto-main-2, required: ooto-main-5 or newer)"
        );
        let status = p.refusal(None);
        assert!(
            status
                .message()
                .contains("client: unknown, required: ooto-main-5 or newer")
        );
    }

    #[test]
    fn default_message_is_neutral() {
        let settings: ClientPolicySettings = toml::from_str("min_build = 1").unwrap();
        let p = ClientPolicy::from_settings(&settings).unwrap();
        assert!(
            p.refusal(None)
                .message()
                .starts_with("This server requires a newer Lore client. (")
        );
    }
}

#[derive(Clone)]
struct Echo;

impl Service<Request<()>> for Echo {
    type Response = Response<()>;
    type Error = Infallible;
    type Future = std::future::Ready<Result<Response<()>, Infallible>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: Request<()>) -> Self::Future {
        std::future::ready(Ok(Status::ok("").into_http()))
    }
}

async fn code_for(policy: Option<Arc<ClientPolicy>>, user_agent: Option<&str>) -> Code {
    let mut service = ClientPolicyLayer::new(policy).layer(Echo);
    let mut request = Request::new(());
    if let Some(ua) = user_agent {
        request
            .headers_mut()
            .insert(USER_AGENT, ua.parse().unwrap());
    }
    let response = service.call(request).await.unwrap();
    Status::from_header_map(response.headers())
        .map(|s| s.code())
        .unwrap_or(Code::Ok)
}

mod layer {
    use super::*;

    #[tokio::test]
    async fn refuses_old_and_passes_new() {
        let p = policy(Some(5));
        assert_eq!(
            code_for(p.clone(), Some("lore/1+ooto-main-4")).await,
            Code::FailedPrecondition
        );
        assert_eq!(code_for(p.clone(), None).await, Code::FailedPrecondition);
        assert_eq!(code_for(p, Some("lore/1+ooto-main-5")).await, Code::Ok);
    }

    /// The gate is mounted on the public server only: with no policy the
    /// layer passes everything, which is what the internal server gets.
    #[tokio::test]
    async fn without_policy_everything_passes() {
        assert_eq!(code_for(None, None).await, Code::Ok);
        assert_eq!(code_for(None, Some("lore/1+local")).await, Code::Ok);
    }
}
