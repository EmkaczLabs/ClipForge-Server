use chrono::{Duration, Utc};
use clipforge_api::{ShareVisibility, UpdateClipRequest, UploadSessionStatus, UserRole};
use clipforge_db::{
    ClipAssetKind, Database, DbError, NewApiToken, NewClip, NewClipAsset, NewUploadSession,
    NewUser, ShareLookupOutcome, UploadMetadata,
};
use tempfile::TempDir;
use uuid::Uuid;

async fn setup_users(db: &Database) -> (String, String) {
    let u1 = db
        .create_user(NewUser {
            id: Uuid::new_v4().to_string(),
            username: "alice".into(),
            display_name: Some("Alice".into()),
            password_hash: "hash_a".into(),
            role: UserRole::User,
            quota_limit_bytes: 1000,
        })
        .await
        .expect("create alice");

    let u2 = db
        .create_user(NewUser {
            id: Uuid::new_v4().to_string(),
            username: "bob".into(),
            display_name: Some("Bob".into()),
            password_hash: "hash_b".into(),
            role: UserRole::User,
            quota_limit_bytes: 1000,
        })
        .await
        .expect("create bob");

    (u1.id, u2.id)
}

#[tokio::test]
async fn persistence_across_db_reopen_and_fk_enforcement() {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("nested").join("clipforge.db");
    let clip_id = Uuid::new_v4();
    let user_id: String;

    {
        let db = Database::open(&db_path).await.expect("open db");
        db.health_check().await.expect("health check");

        let user = db
            .create_user(NewUser {
                id: Uuid::new_v4().to_string(),
                username: "persist_user".into(),
                display_name: None,
                password_hash: "phc".into(),
                role: UserRole::Admin,
                quota_limit_bytes: 5000,
            })
            .await
            .expect("create user");
        user_id = user.id.clone();

        db.create_clip_with_assets(
            NewClip {
                id: clip_id,
                owner_id: user_id.clone(),
                title: "Persisted Clip".into(),
                game: None,
                duration_ms: 1500,
                width: 1920,
                height: 1080,
                fps: 60.0,
                size_bytes: 400,
                codec: "h264".into(),
                content_type: "video/mp4".into(),
                audio_tracks: vec![],
                tags: vec!["highlight".into()],
                notes: Some("nice shot".into()),
                visibility: ShareVisibility::Public,
                created_at: Utc::now(),
                uploaded_at: Utc::now(),
                expires_at: None,
            },
            vec![NewClipAsset {
                id: Uuid::new_v4().to_string(),
                clip_id,
                owner_id: user_id.clone(),
                asset_kind: ClipAssetKind::Original,
                storage_key: format!("clips/{user_id}/{clip_id}/original.mp4"),
                content_type: "video/mp4".into(),
                size_bytes: 400,
                sha256: Some("abc".into()),
            }],
        )
        .await
        .expect("create clip");

        db.set_setting("instance_name", "My Forge")
            .await
            .expect("set setting");

        // Foreign key enforcement: nonexistent owner must fail.
        let fk_err = db
            .create_api_token(NewApiToken {
                id: Uuid::new_v4().to_string(),
                user_id: "nonexistent-user".into(),
                name: "bad".into(),
                prefix: "cf_pat_000".into(),
                token_hash: "deadbeef".into(),
                scopes: vec![],
                expires_at: None,
            })
            .await
            .expect_err("FK must reject nonexistent user");
        assert!(matches!(fk_err, DbError::Sqlite(_)));
    }

    // Reopen database from disk and verify all state survived.
    let reopened = Database::open(&db_path).await.expect("reopen db");
    reopened.health_check().await.expect("health check");
    let user = reopened
        .get_user_by_id(&user_id)
        .await
        .expect("get user")
        .expect("user exists");
    assert_eq!(user.username, "persist_user");
    assert_eq!(user.role, UserRole::Admin);

    let clip = reopened
        .get_clip_for_owner(&user_id, clip_id)
        .await
        .expect("get clip")
        .expect("clip exists");
    assert_eq!(clip.title, "Persisted Clip");

    let usage = reopened.get_storage_usage(&user_id).await.expect("usage");
    assert_eq!(usage.used_bytes, 400);
    assert_eq!(usage.clip_count, 1);

    assert_eq!(
        reopened
            .get_setting("instance_name")
            .await
            .expect("get setting")
            .as_deref(),
        Some("My Forge")
    );
}

#[tokio::test]
async fn cross_user_isolation_for_clips_uploads_tokens_and_shares() {
    let db = Database::open_in_memory().await.expect("open in memory");
    let (alice, bob) = setup_users(&db).await;

    let clip_id = Uuid::new_v4();
    db.create_clip_with_assets(
        NewClip {
            id: clip_id,
            owner_id: alice.clone(),
            title: "Alice Private Clip".into(),
            game: None,
            duration_ms: 1000,
            width: 1280,
            height: 720,
            fps: 30.0,
            size_bytes: 200,
            codec: "h264".into(),
            content_type: "video/mp4".into(),
            audio_tracks: vec![],
            tags: vec![],
            notes: None,
            visibility: ShareVisibility::Private,
            created_at: Utc::now(),
            uploaded_at: Utc::now(),
            expires_at: None,
        },
        vec![NewClipAsset {
            id: Uuid::new_v4().to_string(),
            clip_id,
            owner_id: alice.clone(),
            asset_kind: ClipAssetKind::Original,
            storage_key: format!("clips/{alice}/{clip_id}/original.mp4"),
            content_type: "video/mp4".into(),
            size_bytes: 200,
            sha256: None,
        }],
    )
    .await
    .expect("alice clip");

    let share = db
        .create_share(
            &alice,
            clip_id,
            "share-tok-1".into(),
            ShareVisibility::Private,
            None,
        )
        .await
        .expect("alice share");

    let session = db
        .create_upload_session(NewUploadSession {
            session_id: "sess-alice-1".into(),
            user_id: alice.clone(),
            clip_id: Uuid::new_v4(),
            title: "Uploading".into(),
            filename: "clip.mp4".into(),
            content_type: "video/mp4".into(),
            sha256: None,
            visibility: ShareVisibility::Private,
            metadata: UploadMetadata::default(),
            storage_key: "clips/alice/temp.mp4".into(),
            total_bytes: 300,
            part_size_bytes: 150,
            total_parts: 2,
            expires_at: Utc::now() + Duration::hours(1),
        })
        .await
        .expect("alice upload session");

    let tok = db
        .create_api_token(NewApiToken {
            id: Uuid::new_v4().to_string(),
            user_id: alice.clone(),
            name: "alice-pc".into(),
            prefix: "cf_pat_123".into(),
            token_hash: "hash_alice_tok".into(),
            scopes: vec![],
            expires_at: None,
        })
        .await
        .expect("alice token");

    // Bob cannot read, list, modify, or delete any of Alice's resources.
    assert!(
        db.get_clip_for_owner(&bob, clip_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        db.list_clips_for_owner(&bob, 50, 0)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        db.update_clip_for_owner(
            &bob,
            clip_id,
            UpdateClipRequest {
                title: Some("Hacked".into()),
                ..Default::default()
            }
        )
        .await
        .unwrap()
        .is_none()
    );
    assert!(
        db.delete_clip_for_owner(&bob, clip_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        db.get_clip_asset_for_owner(&bob, clip_id, ClipAssetKind::Original)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        db.list_clip_assets_for_owner(&bob, clip_id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        db.get_upload_session_for_owner(&bob, &session.session_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        db.record_upload_part(&bob, &session.session_id, 1, 150)
            .await,
        Err(DbError::NotFound(_))
    ));
    assert!(matches!(
        db.complete_upload_session(&bob, &session.session_id, 300, None)
            .await,
        Err(DbError::NotFound(_))
    ));
    assert!(
        db.list_shares_for_clip(&bob, clip_id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!db.revoke_share(&bob, &share.share_id).await.unwrap());
    assert!(!db.revoke_api_token(&bob, &tok.id).await.unwrap());
    assert!(db.list_api_tokens_for_user(&bob).await.unwrap().is_empty());

    // Alice's resources remain intact.
    assert!(
        db.get_clip_for_owner(&alice, clip_id)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(db.list_api_tokens_for_user(&alice).await.unwrap().len(), 1);
}

#[tokio::test]
async fn upload_quota_reservation_completion_abort_and_expiry_cleanup() {
    let db = Database::open_in_memory().await.expect("open in memory");
    let (alice, _) = setup_users(&db).await; // quota = 1000 bytes

    // Reserve 600 bytes.
    let s1 = db
        .create_upload_session(NewUploadSession {
            session_id: "s1".into(),
            user_id: alice.clone(),
            clip_id: Uuid::new_v4(),
            title: "Clip 1".into(),
            filename: "c1.mp4".into(),
            content_type: "video/mp4".into(),
            sha256: None,
            visibility: ShareVisibility::Private,
            metadata: UploadMetadata::default(),
            storage_key: "clips/a/1/original.mp4".into(),
            total_bytes: 600,
            part_size_bytes: 300,
            total_parts: 2,
            expires_at: Utc::now() + Duration::hours(1),
        })
        .await
        .expect("s1");

    let usage = db.get_storage_usage(&alice).await.unwrap();
    assert_eq!(usage.used_bytes, 0);
    assert_eq!(usage.remaining_bytes, 400);

    // Attempting to reserve 500 more bytes exceeds 1000-byte quota.
    let err = db
        .create_upload_session(NewUploadSession {
            session_id: "s2-over".into(),
            user_id: alice.clone(),
            clip_id: Uuid::new_v4(),
            title: "Too Big".into(),
            filename: "big.mp4".into(),
            content_type: "video/mp4".into(),
            sha256: None,
            visibility: ShareVisibility::Private,
            metadata: UploadMetadata::default(),
            storage_key: "clips/a/2/original.mp4".into(),
            total_bytes: 500,
            part_size_bytes: 500,
            total_parts: 1,
            expires_at: Utc::now() + Duration::hours(1),
        })
        .await
        .expect_err("quota exceeded");
    assert!(matches!(err, DbError::QuotaExceeded { .. }));

    // Upload parts and complete s1 with actual_size_bytes = 550.
    let s1 = db
        .record_upload_part(&alice, &s1.session_id, 1, 300)
        .await
        .unwrap();
    assert_eq!(s1.status, UploadSessionStatus::Uploading);
    let s1 = db
        .record_upload_part(&alice, &s1.session_id, 2, 250)
        .await
        .unwrap();
    assert_eq!(s1.uploaded_parts, vec![1, 2]);
    assert_eq!(s1.bytes_received, 550);

    let (done_s1, clip1) = db
        .complete_upload_session(&alice, &s1.session_id, 550, Some("sha256hex".into()))
        .await
        .expect("complete s1");
    assert_eq!(done_s1.status, UploadSessionStatus::Completed);
    assert_eq!(clip1.size_bytes, 550);

    let usage = db.get_storage_usage(&alice).await.unwrap();
    assert_eq!(usage.used_bytes, 550);
    assert_eq!(usage.remaining_bytes, 450);
    assert_eq!(usage.clip_count, 1);

    // Reserve 400 bytes and then abort -> quota must be released.
    let s3 = db
        .create_upload_session(NewUploadSession {
            session_id: "s3-abort".into(),
            user_id: alice.clone(),
            clip_id: Uuid::new_v4(),
            title: "Aborted".into(),
            filename: "aborted.mp4".into(),
            content_type: "video/mp4".into(),
            sha256: None,
            visibility: ShareVisibility::Private,
            metadata: UploadMetadata::default(),
            storage_key: "clips/a/3/original.mp4".into(),
            total_bytes: 400,
            part_size_bytes: 400,
            total_parts: 1,
            expires_at: Utc::now() + Duration::hours(1),
        })
        .await
        .expect("s3");
    assert_eq!(
        db.get_storage_usage(&alice).await.unwrap().remaining_bytes,
        50
    );
    let aborted = db
        .abort_upload_session(&alice, &s3.session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(aborted.status, UploadSessionStatus::Aborted);
    assert_eq!(
        db.get_storage_usage(&alice).await.unwrap().remaining_bytes,
        450
    );

    // Expired upload cleanup releases reservation.
    let _s4 = db
        .create_upload_session(NewUploadSession {
            session_id: "s4-expired".into(),
            user_id: alice.clone(),
            clip_id: Uuid::new_v4(),
            title: "Expired".into(),
            filename: "exp.mp4".into(),
            content_type: "video/mp4".into(),
            sha256: None,
            visibility: ShareVisibility::Private,
            metadata: UploadMetadata::default(),
            storage_key: "clips/a/4/original.mp4".into(),
            total_bytes: 300,
            part_size_bytes: 300,
            total_parts: 1,
            expires_at: Utc::now() - Duration::seconds(5),
        })
        .await
        .expect("s4");
    assert_eq!(
        db.get_storage_usage(&alice).await.unwrap().remaining_bytes,
        150
    );

    let cleaned = db.cleanup_expired_uploads(Utc::now()).await.unwrap();
    assert_eq!(cleaned.len(), 1);
    assert_eq!(cleaned[0].session_id, "s4-expired");
    assert_eq!(cleaned[0].status, UploadSessionStatus::Failed);
    assert_eq!(
        db.get_storage_usage(&alice).await.unwrap().remaining_bytes,
        450
    );

    // Deleting clip1 frees all 550 bytes.
    let deleted_assets = db
        .delete_clip_for_owner(&alice, clip1.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(deleted_assets.len(), 1);
    let usage = db.get_storage_usage(&alice).await.unwrap();
    assert_eq!(usage.used_bytes, 0);
    assert_eq!(usage.clip_count, 0);
    assert_eq!(usage.remaining_bytes, 1000);
}

#[tokio::test]
async fn share_lookup_states() {
    let db = Database::open_in_memory().await.expect("open in memory");
    let (alice, _) = setup_users(&db).await;
    let clip_id = Uuid::new_v4();

    db.create_clip_with_assets(
        NewClip {
            id: clip_id,
            owner_id: alice.clone(),
            title: "Shareable".into(),
            game: None,
            duration_ms: 500,
            width: 1920,
            height: 1080,
            fps: 60.0,
            size_bytes: 100,
            codec: "h264".into(),
            content_type: "video/mp4".into(),
            audio_tracks: vec![],
            tags: vec![],
            notes: None,
            visibility: ShareVisibility::Public,
            created_at: Utc::now(),
            uploaded_at: Utc::now(),
            expires_at: None,
        },
        vec![],
    )
    .await
    .unwrap();

    db.create_share(
        &alice,
        clip_id,
        "pub-1".into(),
        ShareVisibility::Public,
        None,
    )
    .await
    .unwrap();
    db.create_share(
        &alice,
        clip_id,
        "priv-1".into(),
        ShareVisibility::Private,
        None,
    )
    .await
    .unwrap();
    db.create_share(
        &alice,
        clip_id,
        "exp-1".into(),
        ShareVisibility::Public,
        Some(Utc::now() - Duration::minutes(1)),
    )
    .await
    .unwrap();
    db.create_share(
        &alice,
        clip_id,
        "rev-1".into(),
        ShareVisibility::Public,
        None,
    )
    .await
    .unwrap();
    assert!(db.revoke_share(&alice, "rev-1").await.unwrap());

    let now = Utc::now();
    assert!(matches!(
        db.lookup_public_share("pub-1", now).await.unwrap(),
        ShareLookupOutcome::Active(_)
    ));
    assert!(matches!(
        db.lookup_public_share("priv-1", now).await.unwrap(),
        ShareLookupOutcome::Private
    ));
    assert!(matches!(
        db.lookup_public_share("exp-1", now).await.unwrap(),
        ShareLookupOutcome::Expired
    ));
    assert!(matches!(
        db.lookup_public_share("rev-1", now).await.unwrap(),
        ShareLookupOutcome::Revoked
    ));
    assert!(matches!(
        db.lookup_public_share("missing", now).await.unwrap(),
        ShareLookupOutcome::NotFound
    ));
}
