#![warn(clippy::all, clippy::pedantic)]

use std::{
    future::IntoFuture,
    str::FromStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, PoisonError, RwLock,
    },
    time::Duration,
};

use expiringmap::ExpiringSet;
use sqlx::{
    query,
    sqlite::{SqliteAutoVacuum, SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    SqlitePool,
};
use tracing_subscriber::{prelude::__tracing_subscriber_SubscriberExt, util::SubscriberInitExt};
use twilight_gateway::{
    error::ReceiveMessageErrorType, CloseFrame, Event, EventTypeFlags, Intents, Shard, ShardId,
    StreamExt,
};
use twilight_http::Client;
use twilight_model::{
    application::{
        command::CommandType,
        interaction::{application_command::CommandData, InteractionData},
    },
    channel::{message::MessageFlags, Message},
    gateway::payload::incoming::{GuildAuditLogEntryCreate, InteractionCreate},
    guild::{audit_log::AuditLogEventType, Permissions},
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    id::{
        marker::{ApplicationMarker, GuildMarker, RoleMarker, UserMarker},
        Id,
    },
};
use twilight_util::builder::{
    command::CommandBuilder, embed::EmbedBuilder, InteractionResponseDataBuilder,
};
use valk_utils::{get_var, parse_var};

#[macro_use]
extern crate tracing;

const GET_PROGRESS_NAME: &str = "Get Role Progress";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let token = get_var("DISCORD_TOKEN");
    let role_id: Id<RoleMarker> = parse_var("ROLE_ID");
    let guild_id: Id<GuildMarker> = parse_var("GUILD_ID");
    let activity_minutes: i64 = parse_var("ACTIVITY_MINUTES");
    let database_url = get_var("DATABASE_URL");

    let db_opts = SqliteConnectOptions::from_str(&database_url)
        .expect("failed to parse DATABASE_URL")
        .create_if_missing(true)
        .optimize_on_close(true, None)
        .auto_vacuum(SqliteAutoVacuum::Incremental)
        .journal_mode(SqliteJournalMode::Wal);
    let db = SqlitePoolOptions::new()
        .min_connections(5)
        .connect_with(db_opts)
        .await
        .unwrap_or_else(|e| panic!("Failed to connect to {database_url}: {e}"));
    sqlx::migrate!()
        .run(&db)
        .await
        .expect("failed to run migrations");

    let intents = Intents::GUILD_MESSAGES | Intents::GUILD_MODERATION;
    let shard = Shard::new(ShardId::ONE, token.clone(), intents);
    let shutdown = Arc::new(AtomicBool::new(false));

    debug!("registering shutdown handler");
    let shutdown_sender = shard.sender();
    let shutdown_copy = Arc::clone(&shutdown);
    tokio::spawn(async move {
        // in the background, wait for shut down signal. then send it into
        // the event loop
        vss::shutdown_signal().await;
        info!("Shutting down, closing discord connection...");
        // Shut down the sender, ignoring the error which occurs if the sender is closed already
        shutdown_sender.close(CloseFrame::NORMAL).ok();
        shutdown_copy.store(true, Ordering::Relaxed);
        info!("Discord connection closed, waiting for cleanup...");
    });

    info!("created shard");
    let http = Arc::new(Client::new(token));
    let xs = Arc::new(RwLock::new(ExpiringSet::new()));
    let db_shutdown = db.clone();
    let my_id = http.current_user_application().await?.model().await?.id;
    let state = AppState {
        http,
        db,
        xs,
        guild_id,
        role_id,
        activity_minutes,
        my_id,
        shut: shutdown,
    };

    let commands = [
        CommandBuilder::new(GET_PROGRESS_NAME, "", CommandType::Message)
            .default_member_permissions(Permissions::MANAGE_ROLES)
            .build(),
        CommandBuilder::new(GET_PROGRESS_NAME, "", CommandType::User)
            .default_member_permissions(Permissions::MANAGE_ROLES)
            .build(),
    ];

    // idempotently set up commands
    state
        .http
        .interaction(state.my_id)
        .set_guild_commands(state.guild_id, &commands)
        .await?;
    // run the loop until the discord events dry up
    event_loop(&state, shard).await;

    // sqlite hates it when you shut down without closing the connection
    db_shutdown.close().await;
    info!("Shutdown complete, bye!");
    Ok(())
}

async fn event_loop(state: &AppState, mut shard: Shard) {
    let event_flags = EventTypeFlags::INTERACTION_CREATE
        | EventTypeFlags::MESSAGE_CREATE
        | EventTypeFlags::GUILD_CREATE
        | EventTypeFlags::GUILD_AUDIT_LOG_ENTRY_CREATE;
    while let Some(next) = shard.next_event(event_flags).await {
        trace!(?next, "got new event");
        let event = match next {
            Ok(event) => event,
            Err(source) => {
                if state.shut.load(Ordering::Relaxed)
                    && matches!(source.kind(), ReceiveMessageErrorType::WebSocket)
                {
                    break;
                }
                error!(?source, "error receiving event");
                continue;
            }
        };
        let state = state.clone();
        match event {
            Event::MessageCreate(mc) => {
                // add the fact that the user sent a message to the db
                wrap_handle(handle_message(mc.0, state)).await;
            }
            Event::InteractionCreate(ic) => {
                // this future was too big. handle interactions
                Box::pin(wrap_handle(handle_interaction(*ic, state))).await;
            }
            Event::GuildAuditLogEntryCreate(alec) => {
                // we listen to audit log events so that when people get kicked
                // we can reset their leveling
                wrap_handle(handle_audit_log(*alec, state)).await;
            }
            Event::GuildCreate(gc) => {
                wrap_handle(async move {
                    // if our guild ID doesn't match the guild
                    // we just got, leave it
                    if gc.id != state.guild_id {
                        debug!("Leaving guild {} ({})", gc.name, gc.id);
                        state.http.leave_guild(gc.id).await?;
                    }
                    Ok(())
                })
                .await;
            }
            Event::GatewayClose(_close) => {
                if state.shut.load(Ordering::Relaxed) {
                    break;
                }
            }
            _ => {}
        }
    }
}

/// Report errors for handler functions, and spawn them into background tasks
#[allow(clippy::unused_async)]
async fn wrap_handle<F: IntoFuture<Output = Result<(), Error>> + Send + 'static>(fut: F)
where
    <F as IntoFuture>::IntoFuture: Send,
{
    tokio::spawn(async {
        if let Err(source) = fut.await {
            match source {
                Error::DiscordApi(source) => warn!(?source),
                _ => {
                    error!(?source);
                }
            }
        }
    });
}

async fn handle_interaction(ic: InteractionCreate, state: AppState) -> Result<(), Error> {
    // if we get an error, report it right in here
    let interaction_response_data = command(&ic, state.clone()).await.unwrap_or_else(|err| {
        InteractionResponseDataBuilder::new()
            .content(format!("Error: {err:?}"))
            .flags(MessageFlags::EPHEMERAL)
            .build()
    });
    let response = InteractionResponse {
        data: Some(interaction_response_data),
        kind: InteractionResponseType::ChannelMessageWithSource,
    };
    // post our interaction back to the discord api with our ID, the interaction ID, and the interaction token
    state
        .http
        .interaction(ic.application_id)
        .create_response(ic.id, &ic.token, &response)
        .await?;
    Ok(())
}

async fn command(
    ic: &InteractionCreate,
    state: AppState,
) -> Result<InteractionResponseData, Error> {
    let Some(InteractionData::ApplicationCommand(data)) = ic.data.as_ref() else {
        return Err(Error::NoInteractionData);
    };
    // command routing is done based on name, discord why
    match data.name.as_str() {
        GET_PROGRESS_NAME => get_progress(data.as_ref(), state).await,
        _ => Err(Error::UnknownCommand),
    }
}

async fn get_progress(
    data: &CommandData,
    state: AppState,
) -> Result<InteractionResponseData, Error> {
    let id = get_target(data)?;
    let id_i64 = id_to_db(id);
    let active_minutes = query!("SELECT id, active_minutes FROM users WHERE id = ?1", id_i64)
        .fetch_optional(&state.db)
        .await?
        .map_or(0, |v| v.active_minutes);
    let msg = if active_minutes == 0 {
        "This user has either not been active, or has reached the activity threshold.".to_string()
    } else {
        format!(
            "user <@{id}> has been active for {active_minutes} minute{}",
            if active_minutes == 1 { "" } else { "s" }
        )
    };

    let embed = EmbedBuilder::new().description(msg).build();
    let ird = InteractionResponseDataBuilder::new()
        .embeds([embed])
        .flags(MessageFlags::EPHEMERAL)
        .build();
    Ok(ird)
}

/// helper function to take a [`CommandData`] that might be a user
/// or a message command and get the ID of the person we need to
/// act upon
fn get_target(data: &CommandData) -> Result<Id<UserMarker>, Error> {
    let id = match data.kind {
        CommandType::User => data.target_id.unwrap().cast(),
        CommandType::Message => {
            data.resolved
                .as_ref()
                .ok_or(Error::NoResolvedData)?
                .messages
                .get(&data.target_id.unwrap().cast())
                .ok_or(Error::NoAuthorResolvedData)?
                .author
                .id
        }
        _ => return Err(Error::UnknownCommandType),
    };
    Ok(id)
}

const EXPIRY_DURATION: Duration = Duration::from_secs(60);

async fn handle_message(mc: Message, state: AppState) -> Result<(), Error> {
    let member = mc.member.ok_or(Error::NoPartialMember)?;
    // immediately ignore this message if:
    // the author is also a bot
    // the message is not in our guild (not ((it's in a guild) and (it's in ours)))
    // the user already has the role we'd be giving them
    // the user still needs to wait to send another message
    let incorrect_guild = !mc.guild_id.is_some_and(|v| v == state.guild_id);
    let has_role = member.roles.contains(&state.role_id);
    let on_cooldown = state.xs.read()?.contains(&mc.author.id);
    if mc.author.bot || incorrect_guild || has_role || on_cooldown {
        debug!(
            author_id = mc.author.id.get(),
            bot = mc.author.bot,
            guild_id = mc.guild_id.map(Id::get),
            expected_guild_id = state.guild_id.get(),
            on_cooldown,
            incorrect_guild,
            has_role,
            "skipped adding XP to user",
        );
        return Ok(());
    }
    let db_id = id_to_db(mc.author.id);
    // insert initial values
    // if this user's values already exist
    // add 1 to active_minutes for that user
    // and return that value
    let q = query!(
        "INSERT INTO users
        (id, active_minutes)
        VALUES (?1, 1)
        ON CONFLICT DO UPDATE SET
        active_minutes = active_minutes + 1
        WHERE id = ?1
        RETURNING active_minutes",
        db_id,
    )
    .fetch_one(&state.db)
    .await?;
    state.xs.write()?.insert(mc.author.id, EXPIRY_DURATION);
    // if they've been active long enough, give them the role
    if q.active_minutes >= state.activity_minutes {
        trace!(
            active = q.active_minutes,
            user = mc.author.id.get(),
            "adding role to user"
        );
        state
            .http
            .add_guild_member_role(state.guild_id, mc.author.id, state.role_id)
            .await?;
        // Once a user has the role, we don't care about them anymore.
        // They already have the role, will lose it when kicked,
        // we don't need to store their info
        delete_user(mc.author.id, &state.db).await?;
    } else {
        trace!(
            active = q.active_minutes,
            user = mc.author.id.get(),
            "skipped adding role to user"
        );
    }
    Ok(())
}

async fn handle_audit_log(
    audit_log: GuildAuditLogEntryCreate,
    state: AppState,
) -> Result<(), Error> {
    trace!(?audit_log, "Got audit log event");
    // if this event:
    // has a target
    // AND
    // the action is Ban or Kick
    // delete them
    if let Some(target) = audit_log.target_id {
        match audit_log.action_type {
            AuditLogEventType::MemberBanAdd | AuditLogEventType::MemberKick => {
                delete_user(target.cast(), &state.db).await?;
            }
            _ => {}
        }
    }
    Ok(())
}

async fn delete_user(target: Id<UserMarker>, db: &SqlitePool) -> Result<(), Error> {
    let id = id_to_db(target);
    // if what this function does defies your gaze, i have some concerns
    query!("DELETE FROM users WHERE id = ?1", id)
        .execute(db)
        .await?;
    Ok(())
}

/// databases hate unsigned ints, so we cast our IDs to i64s as well as hashing them
#[inline]
fn id_to_db<T>(id: Id<T>) -> i64 {
    #[allow(clippy::cast_possible_wrap)]
    let db_id = id.get() as i64;
    db_id
}

#[derive(Clone)]
pub struct AppState {
    pub http: Arc<Client>,
    pub db: SqlitePool,
    pub xs: Arc<RwLock<ExpiringSet<Id<UserMarker>>>>,
    pub guild_id: Id<GuildMarker>,
    pub role_id: Id<RoleMarker>,
    pub activity_minutes: i64,
    pub my_id: Id<ApplicationMarker>,
    pub shut: Arc<AtomicBool>,
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("twilight-http error")]
    DiscordApi(#[from] twilight_http::Error),
    #[error("sqlx error")]
    Sqlx(#[from] sqlx::Error),
    #[error("Lock poisoned")]
    PoisonedLock,
    #[error("Discord did not send the resolved data section of the interaction!")]
    NoResolvedData,
    #[error("Discord did not send a message author matching the target of the interaction!")]
    NoAuthorResolvedData,
    #[error("Discord did not send the target of the interaction!")]
    NoInteractionData,
    #[error("Discord did not send the partial member for this message!")]
    NoPartialMember,
    #[error("Discord did not send guild ID!")]
    NoGuildId,
    #[error("There is no command with this name")]
    UnknownCommand,
    #[error("There is a command with this name, but not with this type!")]
    UnknownCommandType,
}

impl<T> From<PoisonError<T>> for Error {
    fn from(_: PoisonError<T>) -> Self {
        Self::PoisonedLock
    }
}
