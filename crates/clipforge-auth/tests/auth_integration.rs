use chrono::{Duration, Utc};
use clipforge_api::{CreateTokenRequest, UserRole};
use clipforge_auth::{
    AuthError, AuthenticatedUser, authenticate_password, bootstrap_admin, create_user,
    extract_raw_credential, generate_api_token, hash_password, hash_token, issue_api_token,
    list_my_tokens, require_admin, require_owner, require_scope, revoke_my_token, verify_api_token,
    verify_password,
};
use clipforge_db::{Database, NewApiToken};
use uuid::Uuid;

#[test]
fn argon2id_password_hashing_and_redacted_debug() {
    let h1 = hash_password("correct-horse-battery-staple").expect("hash 1");
    let h2 = hash_password("correct-horse-battery-staple").expect("hash 2");
    assert!(h1.starts_with("$argon2id$"));
    assert_ne!(h1, h2, "each hash must use a fresh random salt");
    assert!(verify_password("correct-horse-battery-staple", &h1).unwrap());
    assert!(!verify_password("wrong-password", &h1).unwrap());

    let generated = generate_api_token().expect("generate token");
    let debug_str = format!("{generated:?}");
    assert!(!debug_str.contains(&generated.raw_token));
    assert!(!debug_str.contains(&generated.token_hash));
    assert!(debug_str.contains("[REDACTED]"));
}

#[test]
fn credential_extraction_variants() {
    assert_eq!(
        extract_raw_credential(Some("Bearer cf_pat_abc"), None, None).as_deref(),
        Some("cf_pat_abc")
    );
    assert_eq!(
        extract_raw_credential(Some("bearer cf_pat_xyz"), None, None).as_deref(),
        Some("cf_pat_xyz")
    );
    assert_eq!(
        extract_raw_credential(None, Some("  my-key  "), None).as_deref(),
        Some("my-key")
    );
    assert_eq!(
        extract_raw_credential(None, None, Some("qtok")).as_deref(),
        Some("qtok")
    );
    assert_eq!(extract_raw_credential(Some("Basic abc"), None, None), None);
}

#[tokio::test]
async fn admin_bootstrap_login_and_role_checks() {
    let db = Database::open_in_memory().await.expect("open in memory");

    let admin = bootstrap_admin(&db, "admin", "supersecret123", 10_000)
        .await
        .expect("bootstrap")
        .expect("created admin");
    assert_eq!(admin.role, UserRole::Admin);

    // Second bootstrap is a no-op.
    assert!(
        bootstrap_admin(&db, "admin2", "supersecret123", 10_000)
            .await
            .unwrap()
            .is_none()
    );

    let regular = create_user(
        &db,
        "player1",
        "playerpass123",
        Some("Player One".into()),
        UserRole::User,
        5_000,
    )
    .await
    .expect("create player1");

    // Login succeeds with valid password, fails otherwise.
    let logged_in = authenticate_password(&db, "player1", "playerpass123")
        .await
        .expect("login");
    assert_eq!(logged_in.id, regular.id);
    assert!(matches!(
        authenticate_password(&db, "player1", "wrongpass").await,
        Err(AuthError::InvalidCredentials)
    ));
    assert!(matches!(
        authenticate_password(&db, "nobody", "playerpass123").await,
        Err(AuthError::InvalidCredentials)
    ));

    // Role, scope, and ownership checks.
    let admin_ctx = AuthenticatedUser {
        user_id: admin.id.clone(),
        username: admin.username,
        role: UserRole::Admin,
        scopes: vec![],
    };
    let user_ctx = AuthenticatedUser {
        user_id: regular.id.clone(),
        username: regular.username,
        role: UserRole::User,
        scopes: vec!["clips:read".into()],
    };

    assert!(require_admin(&admin_ctx).is_ok());
    assert!(matches!(
        require_admin(&user_ctx),
        Err(AuthError::Forbidden(_))
    ));
    assert!(require_scope(&user_ctx, "clips:read").is_ok());
    assert!(matches!(
        require_scope(&user_ctx, "clips:write"),
        Err(AuthError::Forbidden(_))
    ));
    assert!(require_owner(&user_ctx, &regular.id).is_ok());
    assert!(matches!(
        require_owner(&user_ctx, &admin.id),
        Err(AuthError::Forbidden(_))
    ));
}

#[tokio::test]
async fn api_token_hashing_expiration_revocation_and_isolation() {
    let db = Database::open_in_memory().await.expect("open in memory");
    let alice = create_user(&db, "alice", "alicepass123", None, UserRole::User, 10_000)
        .await
        .unwrap();
    let bob = create_user(&db, "bob", "bobpass123", None, UserRole::User, 10_000)
        .await
        .unwrap();

    let created = issue_api_token(
        &db,
        &alice.id,
        CreateTokenRequest {
            name: "desktop".into(),
            scopes: vec!["clips:write".into()],
            expires_in_days: Some(30),
        },
    )
    .await
    .expect("issue token");
    assert!(created.token.starts_with("cf_pat_"));

    // Raw token must NOT be stored in DB; only its SHA-256 hash.
    assert!(
        db.find_api_token_by_hash(&created.token)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        db.find_api_token_by_hash(&hash_token(&created.token))
            .await
            .unwrap()
            .is_some()
    );

    let authed = verify_api_token(&db, &created.token)
        .await
        .expect("verify token");
    assert_eq!(authed.user_id, alice.id);
    assert_eq!(authed.scopes, vec!["clips:write"]);

    // Bob cannot see or revoke Alice's token.
    let bob_ctx = AuthenticatedUser {
        user_id: bob.id.clone(),
        username: bob.username.clone(),
        role: UserRole::User,
        scopes: vec![],
    };
    assert!(list_my_tokens(&db, &bob_ctx).await.unwrap().is_empty());
    assert!(!revoke_my_token(&db, &bob_ctx, &created.id).await.unwrap());

    // Alice can list and revoke her own token.
    let alice_tokens = list_my_tokens(&db, &authed).await.unwrap();
    assert_eq!(alice_tokens.len(), 1);
    assert!(alice_tokens[0].last_used_at.is_some());

    assert!(revoke_my_token(&db, &authed, &created.id).await.unwrap());
    assert!(matches!(
        verify_api_token(&db, &created.token).await,
        Err(AuthError::TokenRevoked)
    ));

    // Expired token is rejected.
    let generated = generate_api_token().unwrap();
    db.create_api_token(NewApiToken {
        id: Uuid::new_v4().to_string(),
        user_id: alice.id.clone(),
        name: "expired".into(),
        prefix: generated.prefix,
        token_hash: generated.token_hash,
        scopes: vec![],
        expires_at: Some(Utc::now() - Duration::seconds(10)),
    })
    .await
    .unwrap();
    assert!(matches!(
        verify_api_token(&db, &generated.raw_token).await,
        Err(AuthError::TokenExpired)
    ));
}
