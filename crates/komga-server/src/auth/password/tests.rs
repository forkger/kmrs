use super::*;
use komga_core::model::user::UserRole;

mod behavior;
mod http;

fn checked_password_user() -> (UserAuthentication, KomgaUser) {
    let db = Database::open_in_memory(true).unwrap();
    let migrations = komga_db::main_migrations();
    komga_db::Migrator::new(&migrations, komga_db::Placeholders::default())
        .migrate(&db.rw().unwrap())
        .unwrap();
    let authentication = UserAuthentication::new(db, SessionStore::new(Duration::from_secs(60)));
    let mut user = KomgaUser {
        id: String::new(),
        email: "reader@example.com".into(),
        password: bcrypt::hash("pass", 4).unwrap(),
        roles: [UserRole::Admin].into_iter().collect(),
        shared_all_libraries: true,
        shared_libraries_ids: Default::default(),
        restrictions: Default::default(),
        created_date: komga_core::time_codec::now_utc(),
        last_modified_date: komga_core::time_codec::now_utc(),
    };
    user.id = authentication.users.insert(&user).unwrap();
    (authentication, user)
}

#[tokio::test]
async fn permission_changes_keep_password_checks_but_revoke_sessions() {
    let (mut authentication, user) = checked_password_user();
    authentication.checks = Arc::new(Semaphore::new(1));
    let login = authentication
        .authenticate(user.email.clone(), "pass", None)
        .await
        .unwrap_or_else(|_| panic!("initial password check was refused"));
    let session = login.new_session_id.unwrap();
    let (updated, expired) = authentication
        .update_user(&user.id, "another-admin", |user| {
            user.roles.clear();
            user.shared_all_libraries = false;
            user.restrictions.labels_exclude.insert("private".into());
            Ok(())
        })
        .unwrap();
    assert!(expired);
    assert!(authentication.sessions.get(&session).unwrap().is_none());

    let _held = authentication.checks.clone().acquire_owned().await.unwrap();
    let login = authentication
        .authenticate(user.email, "pass", None)
        .await
        .unwrap_or_else(|_| panic!("permission changes discarded a valid password check"));
    let current = login.auth.unwrap().user;
    assert_eq!(current.roles, updated.roles);
    assert_eq!(current.restrictions, updated.restrictions);
    assert_eq!(current.shared_all_libraries, updated.shared_all_libraries);
    assert_eq!(current.shared_libraries_ids, updated.shared_libraries_ids);
    assert!(login.new_session_id.is_some());
}

#[test]
fn completed_password_check_rejects_a_changed_password() {
    let (authentication, checked_user) = checked_password_user();
    let key = authentication
        .cache
        .key(&checked_user.id, &checked_user.password, None, "pass");
    let mut current = checked_user.clone();
    current.password = bcrypt::hash("newpass", 4).unwrap();
    authentication.users.update(&current).unwrap();

    let outcome = authentication.finish(checked_user, key, None);
    assert!(outcome.auth.is_none());
    assert!(outcome.new_session_id.is_none());
    assert!(!outcome.activity.unwrap().success);
    assert!(authentication.sessions.all().unwrap().is_empty());
    assert!(!authentication.cache.contains(&key));
}

#[test]
fn completed_password_check_rejects_a_deleted_user() {
    let (authentication, checked_user) = checked_password_user();
    let key = authentication
        .cache
        .key(&checked_user.id, &checked_user.password, None, "pass");
    authentication
        .users
        .delete(&checked_user.id, &checked_user.email)
        .unwrap();

    let outcome = authentication.finish(checked_user, key, None);
    assert!(outcome.auth.is_none());
    assert!(outcome.new_session_id.is_none());
    assert!(!outcome.activity.unwrap().success);
    assert!(authentication.sessions.all().unwrap().is_empty());
    assert!(!authentication.cache.contains(&key));
}

#[test]
fn completed_password_check_uses_current_permissions() {
    let (authentication, checked_user) = checked_password_user();
    let key = authentication
        .cache
        .key(&checked_user.id, &checked_user.password, None, "pass");
    let mut current = checked_user.clone();
    current.roles.clear();
    authentication.users.update(&current).unwrap();

    let outcome = authentication.finish(checked_user, key, None);
    assert!(!outcome.auth.unwrap().is_admin());
    assert!(outcome.new_session_id.is_some());
    assert!(outcome.activity.unwrap().success);
}
