use std::{
    io::{self, Write},
    io::ErrorKind,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use matrix_sdk::{
    Client, Error, LoopCtrl, Room, RoomState,
    authentication::matrix::MatrixSession,
    config::SyncSettings,
    encryption::{backups::BackupState, secret_storage::SecretStore},
    room::MessagesOptions,
    ruma::{
        api::client::filter::FilterDefinition,
        events::{AnySyncTimelineEvent, room::message::OriginalSyncRoomMessageEvent},
        RoomAliasId, RoomId,
    },
};
use rand::{RngExt, distr::Alphanumeric, rng};
use serde::{Deserialize, Serialize};
use tokio::fs;
use futures_util::{StreamExt, pin_mut};

/// The data needed to re-build a client.
#[derive(Debug, Serialize, Deserialize)]
struct ClientSession {
    homeserver: String,
    db_path: PathBuf,
    /// The passphrase of the database.
    passphrase: String,
}

#[derive(Debug, Deserialize)]
struct StartupOptions {
    #[serde(rename = "HOMESERVER_URL")]
    homeserver_url: String,
    #[serde(rename = "USER_ID")]
    user_id: String,
    #[serde(rename = "PASSWORD")]
    password: String,
    #[serde(rename = "KEY_BACKUP_PASSPHRASE")]
    key_backup_passphrase: String,
}

/// A room and the amount of time messages should remain in it.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct RoomConfig {
    address: String,
    burn_after_seconds: u64,
}

/// The full session to persist.
#[derive(Debug, Serialize, Deserialize)]
struct FullSession {
    /// The data to re-build the client.
    client_session: ClientSession,

    /// The Matrix user session.
    user_session: MatrixSession,

    /// The latest sync token.
    ///
    /// It is only needed to persist it when using `Client::sync_once()` and we
    /// want to make our syncs faster by not receiving all the initial sync
    /// again.
    #[serde(skip_serializing_if = "Option::is_none")]
    sync_token: Option<String>,

    /// The secret storage key, this key will be used to open the secret-store.
    secret_store_key: String,

    /// Rooms configured for message burning.
    #[serde(default)]
    rooms: Vec<RoomConfig>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing_subscriber::filter::LevelFilter::WARN)
        .init();

    let data_dir = std::env::current_dir()?.join("matrix-burn-bot-rs-appdata");
    if !data_dir.exists() {
        fs::create_dir_all(&data_dir).await?;
    }
    let session_file = data_dir.join("config.toml");

    let (client, secret_store_key, sync_token) = if session_file.exists() {
        restore_session(&session_file).await?
    } else {
        let options = load_startup_options().await?;
        let (client, secret_store_key) = login(&data_dir, &session_file, options.as_ref()).await?;
        (client, secret_store_key, None)
    };
    let room_configs = load_room_configs(&session_file).await?;

    let secret_store = client.encryption().secret_storage().open_secret_store(&secret_store_key).await?;

    let _task = tokio::spawn({
        let client = client.clone();
        async move { listen_for_backup_state_changes(client).await }
    });

    import_known_secrets(&client, secret_store).await?;

    let _cleanup_task = tokio::spawn({
        let client = client.clone();
        async move { cleanup_loop(client, room_configs).await }
    });

    sync(client, sync_token, &session_file).await
}

async fn load_startup_options() -> anyhow::Result<Option<StartupOptions>> {
    match fs::read_to_string("/data/options.json").await {
        Ok(serialized_options) => Ok(Some(serde_json::from_str(&serialized_options)?)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

async fn load_room_configs(session_file: &Path) -> anyhow::Result<Vec<RoomConfig>> {
    let serialized_session = fs::read_to_string(session_file).await?;
    let full_session: FullSession = toml::from_str(&serialized_session)?;

    Ok(full_session.rooms)
}

/// Restore a previous session.
async fn restore_session(session_file: &Path) -> anyhow::Result<(Client, String, Option<String>)> {
    println!("Previous session found in '{}'", session_file.to_string_lossy());

    // The session was serialized as TOML in a file.
    let serialized_session = fs::read_to_string(session_file).await?;
    let FullSession { client_session, user_session, sync_token, secret_store_key, .. } = toml::from_str(&serialized_session)?;

    // Build the client with the previous settings from the session.
    let client = Client::builder()
        .homeserver_url(client_session.homeserver)
        .sqlite_store(client_session.db_path, Some(&client_session.passphrase))
        .build()
        .await?;

    println!("Restoring session for {}…", user_session.meta.user_id);

    // Restore the Matrix user session.
    client.restore_session(user_session).await?;

    Ok((client, secret_store_key, sync_token))
}

/// Login with a new device.
async fn login(
    data_dir: &Path,
    session_file: &Path,
    options: Option<&StartupOptions>,
) -> anyhow::Result<(Client, String)> {
    println!("No previous session found, logging in…");

    let (client, client_session) = build_client(data_dir, options).await?;
    let matrix_auth = client.matrix_auth();
    let (username, password, secret_store_key) = if let Some(options) = options {
        (
            options.user_id.clone(),
            options.password.clone(),
            options.key_backup_passphrase.clone(),
        )
    } else {
        print!("\nUsername: ");
        io::stdout().flush().expect("Unable to write to stdout");
        let mut username = String::new();
        io::stdin().read_line(&mut username).expect("Unable to read user input");

        print!("Password: ");
        io::stdout().flush().expect("Unable to write to stdout");
        let mut password = String::new();
        io::stdin().read_line(&mut password).expect("Unable to read user input");

        print!("Key Backup Passphrase: ");
        io::stdout().flush().expect("Unable to write to stdout");
        let mut secret_store_key = String::new();
        io::stdin().read_line(&mut secret_store_key).expect("Unable to read user input");

        (username.trim().to_owned(), password.trim().to_owned(), secret_store_key.trim().to_owned())
    };

    loop {
        match matrix_auth
            .login_username(&username, &password)
            .initial_device_display_name("matrix-burn-bot-rs")
            .await
        {
            Ok(_) => {
                println!("Logged in as {username}");
                break;
            }
            Err(error) => {
                if options.is_some() {
                    return Err(error.into());
                }
                println!("Error logging in: {error}");
                println!("Please try again\n");
            }
        }
    }

    // Persist the session to reuse it later.
    // This is not very secure, for simplicity. If the system provides a way of
    // storing secrets securely, it should be used instead.
    // Note that we could also build the user session from the login response.
    let user_session = matrix_auth.session().expect("A logged-in client should have a session");
    let serialized_session = toml::to_string(&FullSession {
        client_session,
        user_session,
        sync_token: None,
        secret_store_key: secret_store_key.clone(),
        rooms: Vec::new(),
    })?;
    fs::write(session_file, serialized_session).await?;

    println!("Session persisted in {}", session_file.to_string_lossy());

    // After logging in, you might want to verify this session with another one (see
    // the `emoji_verification` example), or bootstrap cross-signing if this is your
    // first session with encryption, or if you need to reset cross-signing because
    // you don't have access to your old sessions (see the
    // `cross_signing_bootstrap` example).

    Ok((client, secret_store_key))
}

/// Build a new client.
async fn build_client(
    data_dir: &Path,
    options: Option<&StartupOptions>,
) -> anyhow::Result<(Client, ClientSession)> {
    let mut rng = rng();

    // Generating a subfolder for the database is not mandatory, but it is useful if
    // you allow several clients to run at the same time. Each one must have a
    // separate database, which is a different folder with the SQLite store.
    let db_subfolder: String =
        (&mut rng).sample_iter(Alphanumeric).take(7).map(char::from).collect();
    let db_path = data_dir.join(db_subfolder);

    // Generate a random passphrase.
    let passphrase: String =
        (&mut rng).sample_iter(Alphanumeric).take(32).map(char::from).collect();

    // We create a loop here so the user can retry if an error happens.
    loop {
        let homeserver = if let Some(options) = options {
            options.homeserver_url.clone()
        } else {
            let mut homeserver = String::new();
            print!("Homeserver URL: ");
            io::stdout().flush().expect("Unable to write to stdout");
            io::stdin().read_line(&mut homeserver).expect("Unable to read user input");
            homeserver.trim().to_owned()
        };

        println!("\nChecking homeserver…");

        match Client::builder()
            .homeserver_url(&homeserver)
            // We use the SQLite store, which is enabled by default. This is the crucial part to
            // persist the encryption setup.
            // Note that other store backends are available and you can even implement your own.
            .sqlite_store(&db_path, Some(&passphrase))
            .build()
            .await
        {
            Ok(client) => return Ok((client, ClientSession { homeserver, db_path, passphrase})),
            Err(error) if options.is_some() => return Err(error.into()),
            Err(error) => match &error {
                matrix_sdk::ClientBuildError::AutoDiscovery(_)
                | matrix_sdk::ClientBuildError::Url(_)
                | matrix_sdk::ClientBuildError::Http(_) => {
                    println!("Error checking the homeserver: {error}");
                    println!("Please try again\n");
                }
                _ => {
                    // Forward other errors, it's unlikely we can retry with a different outcome.
                    return Err(error.into());
                }
            },
        }
    }
}

/// Setup the client to listen to new messages.
async fn sync(
    client: Client,
    initial_sync_token: Option<String>,
    session_file: &Path,
) -> anyhow::Result<()> {
    println!("Launching a first sync to ignore past messages…");

    // Enable room members lazy-loading, it will speed up the initial sync a lot
    // with accounts in lots of rooms.
    // See <https://spec.matrix.org/v1.6/client-server-api/#lazy-loading-room-members>.
    let filter = FilterDefinition::with_lazy_loading();

    let mut sync_settings = SyncSettings::default().filter(filter.into());

    // We restore the sync where we left.
    // This is not necessary when not using `sync_once`. The other sync methods get
    // the sync token from the store.
    if let Some(sync_token) = initial_sync_token {
        sync_settings = sync_settings.token(sync_token);
    }

    // Let's ignore messages before the program was launched.
    // This is a loop in case the initial sync is longer than our timeout. The
    // server should cache the response and it will ultimately take less time to
    // receive.
    loop {
        match client.sync_once(sync_settings.clone()).await {
            Ok(response) => {
                // This is the last time we need to provide this token, the sync method after
                // will handle it on its own.
                sync_settings = sync_settings.token(response.next_batch.clone());
                persist_sync_token(session_file, response.next_batch).await?;
                break;
            }
            Err(error) => {
                println!("An error occurred during initial sync: {error}");
                println!("Trying again…");
            }
        }
    }

    println!("The client is ready! Listening to new messages…");

    // Now that we've synced, let's attach a handler for incoming room messages.
    client.add_event_handler(on_room_message);

    // This loops until we kill the program or an error happens.
    client
        .sync_with_result_callback(sync_settings, |sync_result| async move {
            let response = sync_result?;

            // We persist the token each time to be able to restore our session
            persist_sync_token(session_file, response.next_batch)
                .await
                .map_err(|err| Error::UnknownError(err.into()))?;

            Ok(LoopCtrl::Continue)
        })
        .await?;

    Ok(())
}

/// Persist the sync token for a future session.
/// Note that this is needed only when using `sync_once`. Other sync methods get
/// the sync token from the store.
async fn persist_sync_token(session_file: &Path, sync_token: String) -> anyhow::Result<()> {
    let serialized_session = fs::read_to_string(session_file).await?;
    let mut full_session: FullSession = toml::from_str(&serialized_session)?;

    full_session.sync_token = Some(sync_token);
    let serialized_session = toml::to_string(&full_session)?;
    fs::write(session_file, serialized_session).await?;

    Ok(())
}


async fn import_known_secrets(client: &Client, secret_store: SecretStore) -> anyhow::Result<()> {
    secret_store.import_secrets().await?;

    let status = client
        .encryption()
        .cross_signing_status()
        .await
        .expect("We should be able to get our cross-signing status");

    if status.is_complete() {
        println!("Successfully imported all the cross-signing keys");
    } else {
        eprintln!("Couldn't import all the cross-signing keys: {status:?}");
    }

    Ok(())
}

async fn listen_for_backup_state_changes(client: Client) {
    let stream = client.encryption().backups().state_stream();
    pin_mut!(stream);

    while let Some(state) = stream.next().await {
        let Ok(state) = state else { panic!("Error while receiving backup state updates") };

        match state {
            BackupState::Unknown => (),
            BackupState::Enabling => println!("Trying to enable backups"),
            BackupState::Resuming => println!("Trying to resume backups"),
            BackupState::Enabled => println!("Backups have been successfully enabled"),
            BackupState::Downloading => println!("Downloading the room keys from the backup"),
            BackupState::Disabling => println!("Disabling the backup"),
            BackupState::Creating => println!("Trying to create a new backup"),
        }
    }
}

async fn cleanup_loop(client: Client, room_configs: Vec<RoomConfig>) {
    let mut interval = tokio::time::interval(Duration::from_secs(600));
    interval.tick().await;

    loop {
        interval.tick().await;

        for room_config in &room_configs {
            let Some(room) = configured_room(&client, &room_config.address).await else {
                continue;
            };

            if room.state() != RoomState::Joined {
                continue;
            }

            cleanup_room(&room, room_config.burn_after_seconds).await;
        }
    }
}

async fn configured_room(client: &Client, address: &str) -> Option<Room> {
    if let Ok(room_id) = RoomId::parse(address) {
        return client.get_room(&room_id);
    }

    let alias = RoomAliasId::parse(address).ok()?;
    let room_id = client.resolve_room_alias(&alias).await.ok()?.room_id;

    client.get_room(&room_id)
}

async fn cleanup_room(room: &Room, burn_after_seconds: u64) {
    let burn_after = Duration::from_secs(burn_after_seconds);
    let mut from = None;

    loop {
        let mut options = MessagesOptions::backward();
        options.from = from.clone();
        options.limit = 100u32.into();

        let messages = match room.messages(options).await {
            Ok(messages) => messages,
            Err(_) => return,
        };

        for event in messages.chunk {
            cleanup_event(room, event, burn_after).await;
        }

        let Some(next_from) = messages.end else { break };
        if from.as_deref() == Some(next_from.as_str()) {
            break;
        }
        from = Some(next_from);
    }
}

async fn cleanup_event(
    room: &Room,
    event: matrix_sdk::deserialized_responses::TimelineEvent,
    burn_after: Duration,
) {
    let Some(timestamp) = event.timestamp else { return };
    let Some(timestamp) = timestamp.to_system_time() else { return };
    let Ok(age) = SystemTime::now().duration_since(timestamp) else { return };
    if age < burn_after {
        return;
    }

    let Ok(AnySyncTimelineEvent::MessageLike(message_like)) = event.kind.raw().deserialize() else {
        return;
    };
    if message_like.is_redacted() || message_like.event_type().to_string() == "m.room.redaction" {
        return;
    }

    let event_id = message_like.event_id();
    let reason = format!(
        "Message expired because it is {} seconds old (limit: {})",
        age.as_secs(),
        burn_after.as_secs(),
    );
    if room.redact(event_id, Some(&reason), None).await.is_ok() {
        println!(
            "got deleted event {} from room {} because it was {} seconds old",
            event_id,
            room.room_id(),
            age.as_secs(),
        );
    }
}

/// Handle room messages.
async fn on_room_message(event: OriginalSyncRoomMessageEvent, room: Room) {
    // We only want to log text messages in joined rooms.
    if room.state() != RoomState::Joined {
        return;
    }
    // let MessageType::Text(text_content) = &event.content.msgtype else { return };

    let room_name = match room.display_name().await {
        Ok(room_name) => room_name.to_string(),
        Err(error) => {
            println!("Error getting room display name: {error}");
            "No room name".to_string()
        }
    };

    let ts = event.origin_server_ts; // ruma::Timestamp (ms since unix epoch)
    let system_time = ts.to_system_time(); // std::time::SystemTime

    println!("Got a new message in room {} [{}], event time: {:?}", room_name, room.room_id(), system_time);

}
