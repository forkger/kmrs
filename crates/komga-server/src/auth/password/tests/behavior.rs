use super::*;

fn authentication_with_checks(permits: usize) -> (UserAuthentication, KomgaUser) {
    let (mut authentication, user) = checked_password_user();
    authentication.checks = Arc::new(Semaphore::new(permits));
    (authentication, user)
}

async fn login(
    authentication: &UserAuthentication,
    email: &str,
    password: &str,
    client: Option<IpAddr>,
) -> Outcome {
    authentication
        .authenticate(email.into(), password, client)
        .await
        .unwrap_or_else(|_| panic!("password checks unexpectedly saturated"))
}

fn verified_session(outcome: Outcome) -> String {
    assert!(outcome.auth.is_some());
    assert!(outcome.activity.unwrap().success);
    outcome
        .new_session_id
        .expect("a successful login's session")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_wrong_passwords_cannot_pass_the_lock() {
    const PERMITS: usize = 4;
    let (authentication, user) = authentication_with_checks(PERMITS);
    let authentication = Arc::new(authentication);
    let attempts: Vec<_> = (0..40)
        .map(|_| {
            let authentication = authentication.clone();
            let email = user.email.clone();
            tokio::spawn(async move {
                let outcome = login(&authentication, &email, "wrong", None).await;
                assert!(outcome.auth.is_none());
                assert!(outcome.new_session_id.is_none());
                let activity = outcome.activity.unwrap();
                assert!(!activity.success);
                activity.error.unwrap()
            })
        })
        .collect();
    let mut checked = 0;
    let mut refused = 0;
    for attempt in attempts {
        match attempt.await.unwrap().as_str() {
            "Bad credentials" => checked += 1,
            "Too many failed attempts" => refused += 1,
            error => panic!("unexpected failure: {error}"),
        }
    }
    assert_eq!(checked + refused, 40);
    assert!(
        (5..=5 + PERMITS - 1).contains(&checked),
        "{checked} of 40 wrong guesses were checked"
    );
}

#[tokio::test]
async fn a_waiting_password_check_runs_once_a_permit_is_free() {
    let (authentication, user) = authentication_with_checks(1);
    let held = authentication.checks.clone().acquire_owned().await.unwrap();
    let waiting = authentication.authenticate(user.email, "pass", None);
    tokio::pin!(waiting);
    assert!(futures_util::poll!(waiting.as_mut()).is_pending());
    drop(held);
    verified_session(
        waiting
            .await
            .unwrap_or_else(|_| panic!("a released permit was not used")),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_burst_of_correct_passwords_is_never_refused() {
    let (authentication, user) = authentication_with_checks(8);
    let authentication = Arc::new(authentication);
    let attempts: Vec<_> = (0..12)
        .map(|_| {
            let authentication = authentication.clone();
            let email = user.email.clone();
            tokio::spawn(async move {
                verified_session(login(&authentication, &email, "pass", None).await)
            })
        })
        .collect();
    for attempt in attempts {
        assert!(!attempt.await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn a_repeated_correct_password_needs_no_check() {
    let (authentication, user) = authentication_with_checks(1);
    let home = Some("203.0.113.9".parse().unwrap());
    let other = Some("198.51.100.7".parse().unwrap());
    verified_session(login(&authentication, &user.email, "pass", home).await);
    let held = authentication.checks.clone().acquire_owned().await.unwrap();
    verified_session(login(&authentication, &user.email, "pass", home).await);
    assert_eq!(authentication.sessions.all().unwrap().len(), 2);
    verified_session(login(&authentication, "READER@example.com", "pass", home).await);
    assert!(authentication
        .authenticate(user.email.clone(), "wrong", home)
        .await
        .is_err());
    assert!(authentication
        .authenticate(user.email.clone(), "pass", other)
        .await
        .is_err());

    drop(held);
    let failure = login(&authentication, &user.email, "wrong", home).await;
    assert!(failure.auth.is_none());
    assert_eq!(
        failure.activity.unwrap().error.as_deref(),
        Some("Bad credentials")
    );
    let _held = authentication.checks.clone().acquire_owned().await.unwrap();
    assert!(authentication
        .authenticate(user.email, "wrong", home)
        .await
        .is_err());
}

#[tokio::test]
async fn a_login_without_a_check_clears_the_failures_too() {
    let (authentication, user) = authentication_with_checks(1);
    verified_session(login(&authentication, &user.email, "pass", None).await);
    for _ in 0..2 {
        for _ in 0..4 {
            let failure = login(&authentication, &user.email, "wrong", None).await;
            assert!(failure.auth.is_none());
            assert_eq!(
                failure.activity.unwrap().error.as_deref(),
                Some("Bad credentials")
            );
        }
        let _held = authentication.checks.clone().acquire_owned().await.unwrap();
        verified_session(login(&authentication, &user.email, "pass", None).await);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_first_burst_is_served_from_its_first_check() {
    let (authentication, mut user) = authentication_with_checks(1);
    user.password = bcrypt::hash("pass", 10).unwrap();
    authentication.users.update(&user).unwrap();
    let authentication = Arc::new(authentication);
    let held = authentication.checks.clone().acquire_owned().await.unwrap();
    let entered = Arc::new(tokio::sync::Barrier::new(31));
    let attempts: Vec<_> = (0..30)
        .map(|_| {
            let authentication = authentication.clone();
            let email = user.email.clone();
            let entered = entered.clone();
            tokio::spawn(async move {
                let waiting = authentication.authenticate(email, "pass", None);
                tokio::pin!(waiting);
                assert!(futures_util::poll!(waiting.as_mut()).is_pending());
                entered.wait().await;
                verified_session(
                    waiting
                        .await
                        .unwrap_or_else(|_| panic!("cold burst was refused")),
                )
            })
        })
        .collect();
    entered.wait().await;
    drop(held);
    for attempt in attempts {
        assert!(!attempt.await.unwrap().is_empty());
    }
    assert_eq!(authentication.checks.available_permits(), 1);
}

#[tokio::test]
async fn wrong_passwords_for_unknown_accounts_are_not_tracked() {
    let (authentication, user) = authentication_with_checks(1);
    for number in 0..6 {
        let failure = login(
            &authentication,
            &format!("nobody{number}@example.com"),
            "wrong",
            None,
        )
        .await;
        assert!(failure.auth.is_none());
        assert_eq!(
            failure.activity.unwrap().error.as_deref(),
            Some("Bad credentials")
        );
    }
    assert_eq!(authentication.throttle.len(), 0);
    verified_session(login(&authentication, &user.email, "pass", None).await);
}

#[tokio::test]
async fn password_reset_rejects_cached_credentials_and_revokes_sessions() {
    let (authentication, user) = authentication_with_checks(1);
    let session = verified_session(login(&authentication, &user.email, "pass", None).await);
    let password = bcrypt::hash("newpass", 4).unwrap();
    let (_, expired) = authentication
        .update_user(&user.id, "another-admin", |user| {
            user.password = password;
            Ok(())
        })
        .unwrap();
    assert!(expired);
    assert!(authentication.sessions.get(&session).unwrap().is_none());
    let failure = login(&authentication, &user.email, "pass", None).await;
    assert!(failure.auth.is_none());
    assert!(failure.new_session_id.is_none());
    assert_eq!(
        failure.activity.unwrap().error.as_deref(),
        Some("Bad credentials")
    );
    verified_session(login(&authentication, &user.email, "newpass", None).await);

    let _held = authentication.checks.clone().acquire_owned().await.unwrap();
    assert!(authentication
        .authenticate(user.email, "pass", None)
        .await
        .is_err());
}

#[tokio::test]
async fn deleting_a_user_rejects_cached_credentials_and_revokes_sessions() {
    let (authentication, user) = authentication_with_checks(1);
    let session = verified_session(login(&authentication, &user.email, "pass", None).await);
    authentication.delete_user(&user.id).unwrap();
    assert!(authentication.sessions.get(&session).unwrap().is_none());
    let _held = authentication.checks.clone().acquire_owned().await.unwrap();
    let failure = login(&authentication, &user.email, "pass", None).await;
    assert!(failure.auth.is_none());
    assert!(failure.new_session_id.is_none());
    assert_eq!(
        failure.activity.unwrap().error.as_deref(),
        Some("Bad credentials")
    );
}
