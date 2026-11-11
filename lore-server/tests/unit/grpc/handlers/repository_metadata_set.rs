// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_base::types::RepositoryId;
use lore_proto::RepositoryMetadataSetRequest;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryMetadata;
use lore_server::authnz::repository_authorizer::Action;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::grpc::handlers::repository_metadata_set::*;
use tonic::Code;
use tonic::Request;
use tonic::Status;

use crate::store::test_support::test_store_create;

const REPOSITORY_ID: [u8; 16] = [1u8; 16];

/// Denies every request.
struct DenyAllRepositoryAuthorizer;

#[async_trait::async_trait]
impl RepositoryAuthorizer for DenyAllRepositoryAuthorizer {
    async fn check_repository_access(
        &self,
        _token: Option<&lore_server::authnz::repository_authorizer::VerifiedToken<'_>>,
        _repository_id: RepositoryId,
        _action: Action,
    ) -> Result<(), Status> {
        Err(Status::permission_denied("denied"))
    }
}

/// Permits, recording the action the handler asked.
#[derive(Default)]
struct RecordingPermitAuthorizer {
    asked: std::sync::Mutex<Option<String>>,
}

#[async_trait::async_trait]
impl RepositoryAuthorizer for RecordingPermitAuthorizer {
    async fn check_repository_access(
        &self,
        _token: Option<&lore_server::authnz::repository_authorizer::VerifiedToken<'_>>,
        _repository_id: RepositoryId,
        action: Action,
    ) -> Result<(), Status> {
        self.asked.lock().unwrap().replace(action.to_string());
        Ok(())
    }
}

async fn seed_metadata_blob(
    immutable: Arc<dyn lore_storage::ImmutableStore>,
    mutable: Arc<dyn lore_storage::MutableStore>,
) -> lore_base::types::Hash {
    let repo_ctx = Arc::new(RepositoryContext::new_server_context(
        immutable,
        mutable,
        Context::from(REPOSITORY_ID).into(),
    ));
    lore_revision::repository::metadata_store(
        repo_ctx,
        RepositoryMetadata {
            name: "test".to_string(),
            ..Default::default()
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn no_auth_configured_allows_operation() {
    let (immutable, mutable, execution) = test_store_create().await.unwrap();
    LORE_CONTEXT
        .scope(execution, async move {
            let hash = seed_metadata_blob(immutable.clone(), mutable.clone()).await;
            let request = Request::new(RepositoryMetadataSetRequest {
                repository_id: REPOSITORY_ID.to_vec().into(),
                expected_hash: vec![0u8; 32].into(),
                new_hash: hash.into(),
            });
            handler(
                request,
                Arc::new(AllowAllRepositoryAuthorizer),
                immutable,
                mutable,
            )
            .await
            .unwrap();
        })
        .await;
}

#[tokio::test]
async fn auth_configured_no_access_returns_permission_denied() {
    let (immutable, mutable, _) = test_store_create().await.unwrap();
    let request = Request::new(RepositoryMetadataSetRequest {
        repository_id: REPOSITORY_ID.to_vec().into(),
        expected_hash: vec![0u8; 32].into(),
        new_hash: vec![1u8; 32].into(),
    });
    let err = handler(
        request,
        Arc::new(DenyAllRepositoryAuthorizer),
        immutable,
        mutable,
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), Code::PermissionDenied);
    assert_eq!(err.message(), "Unauthorized");
}

#[tokio::test]
async fn auth_configured_with_access_allows_operation() {
    let (immutable, mutable, execution) = test_store_create().await.unwrap();
    LORE_CONTEXT
        .scope(execution, async move {
            let hash = seed_metadata_blob(immutable.clone(), mutable.clone()).await;
            let authorizer = Arc::new(RecordingPermitAuthorizer::default());
            let request = Request::new(RepositoryMetadataSetRequest {
                repository_id: REPOSITORY_ID.to_vec().into(),
                expected_hash: vec![0u8; 32].into(),
                new_hash: hash.into(),
            });
            handler(request, authorizer.clone(), immutable, mutable)
                .await
                .unwrap();
            assert_eq!(authorizer.asked.lock().unwrap().as_deref(), Some("write"));
        })
        .await;
}

/// Permits only what a `label` token holds: read and label.
struct LabelOnlyAuthorizer;

#[async_trait::async_trait]
impl RepositoryAuthorizer for LabelOnlyAuthorizer {
    async fn check_repository_access(
        &self,
        _token: Option<&lore_server::authnz::repository_authorizer::VerifiedToken<'_>>,
        _repository_id: RepositoryId,
        action: Action,
    ) -> Result<(), Status> {
        match action {
            Action::Read | Action::Label => Ok(()),
            _ => Err(Status::permission_denied("denied")),
        }
    }
}

async fn set_with(
    authorizer: Arc<dyn RepositoryAuthorizer>,
    keys: &[&str],
) -> Result<bool, Status> {
    let (immutable, mutable, execution) = test_store_create().await.unwrap();
    let keys = keys.to_vec();
    LORE_CONTEXT
        .scope(execution, async move {
            let repo_ctx = Arc::new(RepositoryContext::new_server_context(
                immutable.clone(),
                mutable.clone(),
                Context::from(REPOSITORY_ID).into(),
            ));
            let mut metadata = lore_revision::metadata::Metadata::new();
            for key in keys {
                metadata.set_string(key, "value").unwrap();
            }
            let hash = metadata.serialize(repo_ctx).await.unwrap();
            let request = Request::new(RepositoryMetadataSetRequest {
                repository_id: REPOSITORY_ID.to_vec().into(),
                expected_hash: vec![0u8; 32].into(),
                new_hash: hash.into(),
            });
            handler(request, authorizer, immutable, mutable)
                .await
                .map(|response| response.into_inner().success)
        })
        .await
}

#[tokio::test]
async fn label_token_sets_label_key() {
    assert!(
        set_with(Arc::new(LabelOnlyAuthorizer), &["label/Android_1.0.1"])
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn label_token_refused_for_other_key() {
    let err = set_with(Arc::new(LabelOnlyAuthorizer), &["description"])
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::PermissionDenied);
}

#[tokio::test]
async fn label_token_refused_for_mixed_keys() {
    let err = set_with(Arc::new(LabelOnlyAuthorizer), &["label/x", "description"])
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::PermissionDenied);
}

#[tokio::test]
async fn write_token_still_sets_any_key() {
    assert!(
        set_with(
            Arc::new(AllowAllRepositoryAuthorizer),
            &["label/x", "description"]
        )
        .await
        .unwrap()
    );
}

mod label_only_change {
    use bytes::Bytes;
    use lore_revision::metadata::Metadata;

    use super::*;

    const STRING: u32 = lore_revision::metadata::MetadataType::String as u32;

    /// A metadata blob with exactly these (key, value, raw tag) entries, in order, however
    /// malformed: no setter can express a repeated key or an unknown tag.
    async fn blob(entries: &[(&str, &str, u32)]) -> Metadata {
        let (immutable, mutable, execution) = test_store_create().await.unwrap();
        LORE_CONTEXT
            .scope(execution, async move {
                let repo = Arc::new(RepositoryContext::new_server_context(
                    immutable,
                    mutable,
                    Context::from(REPOSITORY_ID).into(),
                ));
                let mut raw = Vec::new();
                raw.extend_from_slice(&0x6D657461u32.to_ne_bytes());
                raw.extend_from_slice(&1u32.to_ne_bytes());
                for (key, value, tag) in entries {
                    raw.extend_from_slice(&(key.len() as u32).to_ne_bytes());
                    raw.extend_from_slice(&(value.len() as u32).to_ne_bytes());
                    raw.extend_from_slice(&tag.to_ne_bytes());
                    raw.extend_from_slice(key.as_bytes());
                    raw.extend_from_slice(value.as_bytes());
                }
                let address = lore_revision::immutable::write_with_tracker(
                    repo.clone(),
                    Context::default(),
                    Bytes::from(raw),
                    lore_revision::immutable::write_options_from_repository(repo.clone())
                        .with_local_cache_priority(),
                    None,
                )
                .await
                .unwrap();
                Metadata::deserialize(repo, address.hash).await.unwrap()
            })
            .await
    }

    #[tokio::test]
    async fn only_label_entries_changed_is_allowed() {
        let current = blob(&[("name", "a", STRING), ("label/x", "1", STRING)]).await;
        let proposed = blob(&[
            ("name", "a", STRING),
            ("label/x", "2", STRING),
            ("label/y", "3", STRING),
        ])
        .await;
        assert!(label_only_change(&current, &proposed));
        assert!(label_only_change(
            &Metadata::new(),
            &proposed_labels().await
        ));
    }

    async fn proposed_labels() -> Metadata {
        blob(&[("label/x", "1", STRING)]).await
    }

    #[tokio::test]
    async fn duplicate_keys_are_refused() {
        let current = blob(&[("name", "a", STRING)]).await;
        // A reader takes the first `name`, so the diff must not take the last.
        let proposed = blob(&[("name", "b", STRING), ("name", "a", STRING)]).await;
        assert!(!label_only_change(&current, &proposed));
        let proposed = blob(&[
            ("name", "a", STRING),
            ("label/x", "1", STRING),
            ("label/x", "2", STRING),
        ])
        .await;
        assert!(!label_only_change(&current, &proposed));
    }

    #[tokio::test]
    async fn unknown_tag_is_refused() {
        let current = blob(&[("name", "a", STRING)]).await;
        let proposed = blob(&[("name", "x", 99), ("name", "a", STRING)]).await;
        assert!(!label_only_change(&current, &proposed));
        let proposed = blob(&[("name", "a", STRING), ("label/x", "1", 99)]).await;
        assert!(!label_only_change(&current, &proposed));
    }

    #[tokio::test]
    async fn non_label_entry_reordered_changed_added_or_removed_is_refused() {
        let current = blob(&[("a", "1", STRING), ("b", "2", STRING)]).await;
        for proposed in [
            vec![("b", "2", STRING), ("a", "1", STRING)],
            vec![("a", "1", STRING), ("b", "3", STRING)],
            vec![("a", "1", STRING), ("b", "2", STRING), ("c", "4", STRING)],
            vec![("a", "1", STRING)],
        ] {
            assert!(!label_only_change(&current, &blob(&proposed).await));
        }
    }
}
