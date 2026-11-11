// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A `label` token (read + label, no write) is refused everything that mutates anything but
//! `label/` repository metadata and plain Put.
use std::sync::Arc;

use lore_base::types::Context;
use lore_base::types::RepositoryId;
use lore_proto::lore::storage::v1 as storage_v1;
use lore_server::authnz::repository_authorizer::Action;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::VerifiedToken;
use lore_server::grpc::handlers::branch_push::PushAccess;
use lore_server::hooks::HookDispatcher;
use lore_telemetry::InstrumentProvider;
use tonic::Code;
use tonic::Request;
use tonic::Status;

use crate::notification::testing::MockNotificationSender;
use crate::store::test_support::test_store_create;

struct LabelOnlyAuthorizer;

#[async_trait::async_trait]
impl RepositoryAuthorizer for LabelOnlyAuthorizer {
    async fn check_repository_access(
        &self,
        _token: Option<&VerifiedToken<'_>>,
        _repository_id: RepositoryId,
        action: Action,
    ) -> Result<(), Status> {
        match action {
            Action::Read | Action::Label => Ok(()),
            _ => Err(Status::permission_denied("denied")),
        }
    }
}

#[derive(Clone)]
struct TestInstrumentProvider;

impl InstrumentProvider for TestInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "test"
    }
}

fn authorizer() -> Arc<dyn RepositoryAuthorizer> {
    Arc::new(LabelOnlyAuthorizer)
}

fn repository() -> Context {
    Context::from([7u8; 16])
}

fn request<T>(inner: T) -> Request<T> {
    let mut request = Request::new(inner);
    request.metadata_mut().insert_bin(
        lore_transport::grpc::PARTITION_ID_KEY,
        tonic::metadata::BinaryMetadataValue::from_bytes(repository().data()),
    );
    request
}

fn streaming<T: Default + tonic_prost::prost::Message + 'static>() -> Request<tonic::Streaming<T>> {
    request(tonic::Streaming::new_request(
        tonic_prost::ProstDecoder::<T>::default(),
        tonic::body::Body::empty(),
        None,
        None,
    ))
}

fn denied<T>(result: Result<T, Status>) {
    assert_eq!(
        result.err().expect("must be refused").code(),
        Code::PermissionDenied
    );
}

#[tokio::test]
async fn branch_create_refused() {
    let (immutable, mutable, _) = test_store_create().await.unwrap();
    denied(
        lore_server::grpc::handlers::branch_create::handler(
            request(lore_proto::BranchCreateRequest::default()),
            &authorizer(),
            immutable,
            mutable,
            Arc::new(MockNotificationSender::new()),
            &Arc::new(HookDispatcher::empty()),
            &TestInstrumentProvider,
        )
        .await,
    );
}

#[tokio::test]
async fn branch_metadata_set_refused() {
    let (immutable, mutable, _) = test_store_create().await.unwrap();
    denied(
        lore_server::grpc::handlers::branch_metadata_set::handler(
            request(lore_proto::BranchMetadataSetRequest::default()),
            &authorizer(),
            immutable,
            mutable,
        )
        .await,
    );
}

#[tokio::test]
async fn branch_push_refused() {
    let access = PushAccess::of(
        &authorizer(),
        &tonic::Extensions::new(),
        repository().into(),
    )
    .await;
    denied(access);
}

#[tokio::test]
async fn mutable_compare_and_swap_refused() {
    let (_, mutable, _) = test_store_create().await.unwrap();
    denied(
        lore_server::grpc::storage::v1::mutable_compare_and_swap::handler(
            request(storage_v1::MutableCompareAndSwapRequest::default()),
            &authorizer(),
            mutable,
        )
        .await,
    );
}

#[tokio::test]
async fn copy_refused() {
    let (immutable, _, _) = test_store_create().await.unwrap();
    denied(
        lore_server::grpc::storage::v1::copy::handler(
            streaming::<storage_v1::CopyRequest>(),
            immutable,
            authorizer(),
            &TestInstrumentProvider,
        )
        .await,
    );
}

#[tokio::test]
async fn put_resolved_refused() {
    let (immutable, mutable, _) = test_store_create().await.unwrap();
    denied(
        lore_server::grpc::storage::v1::put_resolved::handler(
            streaming::<storage_v1::PutResolvedRequest>(),
            &authorizer(),
            mutable,
            immutable,
            &TestInstrumentProvider,
        )
        .await,
    );
}

#[tokio::test]
async fn put_is_permitted() {
    let (immutable, _, _) = test_store_create().await.unwrap();
    // Reaches past the action check: an empty stream is answered, not refused.
    lore_server::grpc::storage::v1::put::handler(
        streaming::<storage_v1::PutRequest>(),
        &authorizer(),
        immutable,
        &TestInstrumentProvider,
    )
    .await
    .expect("label may Put");
}
