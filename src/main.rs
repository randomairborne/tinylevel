#![warn(clippy::all, clippy::pedantic)]

use std::{
    future::IntoFuture,
    str::FromStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use sqlx::{
    query,
    sqlite::{SqliteAutoVacuum, SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    SqlitePool,
};
use tokio_util::task::TaskTracker;
use tracing::level_filters::LevelFilter;
use twilight_gateway::{
    error::ReceiveMessageErrorType, CloseFrame, Event, EventTypeFlags, Intents, Shard, ShardId,
    StreamExt,
};
use twilight_http::Client;
use twilight_model::{
    application::{
        command::{Command, CommandOption, CommandOptionType, CommandType},
        interaction::{
            application_command::{CommandData, CommandOptionValue},
            InteractionContextType, InteractionData,
        },
    },
    channel::{message::MessageFlags, Message},
    gateway::payload::incoming::InteractionCreate,
    guild::Permissions,
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    id::{
        marker::{ApplicationMarker, GuildMarker, RoleMarker, UserMarker},
        Id,
    },
};
use twilight_util::builder::{
    command::CommandBuilder, embed::EmbedBuilder, InteractionResponseDataBuilder,
};
use valk_utils::{get_var, parse_var, parse_var_or};

#[macro_use]
extern crate tracing;

const GET_PROGRESS_NAME_CTX: &str = "Get Role Progress";
const RESET_PROGRESS_NAME_CTX: &str = "Reset Role Progress";

const GET_PROGRESS_NAME_SLASH: &str = "progress";
const RESET_PROGRESS_NAME_SLASH: &str = "reset";

const SLASH_ARG_NAME: &str = "name";

#[tokio::main]
async fn main() {
    let log_level: LevelFilter = parse_var_or("LOG_LEVEL", LevelFilter::INFO);
    tracing_subscriber::fmt()
        .with_max_level(log_level)
        .json()
        .init();

    let token = get_var("DISCORD_TOKEN");
    let role_id: Id<RoleMarker> = parse_var("ROLE_ID");
    let guild_id: Id<GuildMarker> = parse_var("GUILD_ID");
    let activity_minutes: i64 = parse_var("ACTIVITY_MINUTES");
    let cooldown_seconds: i64 = parse_var("COOLDOWN_SECONDS");
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

    let shard = Shard::new(ShardId::ONE, token.clone(), Intents::GUILD_MESSAGES);
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
        shutdown_copy.store(true, Ordering::Release);
        info!("Discord connection closed, waiting for cleanup...");
    });

    info!("created shard");
    let http = Arc::new(Client::new(token));
    let db_shutdown = db.clone();
    let me = http
        .current_user_application()
        .await
        .expect("Failed to fetch own ID")
        .model()
        .await
        .expect("Failed to deserialize own ID");
    let state = AppState {
        http,
        db,
        guild_id,
        role_id,
        activity_minutes,
        cooldown_seconds,
        app_id: me.id,
        bot_id: me.bot.expect("No associated bot").id,
        shutdown,
    };

    let commands = get_commands();

    // idempotently set up commands
    state
        .http
        .interaction(state.app_id)
        .set_guild_commands(state.guild_id, &commands)
        .await
        .expect("Failed to set guild commands for bot");

    // run the loop until the discord events dry up
    event_loop(&state, shard).await;

    // sqlite hates it when you shut down without closing the connection
    db_shutdown.close().await;
    info!("Shutdown complete, bye!");
}

async fn event_loop(state: &AppState, mut shard: Shard) {
    let event_flags = EventTypeFlags::INTERACTION_CREATE | EventTypeFlags::MESSAGE_CREATE;
    // The task tracker allows us to ensure all the current messages are handled before we shut down
    let task_tracker = TaskTracker::new();
    let runtime = tokio::runtime::Handle::current();

    while let Some(next) = shard.next_event(event_flags).await {
        trace!(?next, "got new event");
        let event = match next {
            Ok(event) => event,
            Err(source) => {
                if matches!(source.kind(), ReceiveMessageErrorType::Reconnect)
                    && state.shutdown.load(Ordering::Acquire)
                {
                    // If we have a gateway error and are shutting down
                    // Generate a fake gateway close
                    Event::GatewayClose(None)
                } else {
                    error!(?source, "error receiving event");
                    continue;
                }
            }
        };
        let state = state.clone();
        match event {
            Event::MessageCreate(mc) => {
                // add the fact that the user sent a message to the db
                wrap_handle(&runtime, &task_tracker, handle_message(mc.0, state));
            }
            Event::InteractionCreate(ic) => {
                // handle commands
                wrap_handle(&runtime, &task_tracker, handle_interaction(*ic, state));
            }
            Event::GatewayClose(_close) => {
                if state.shutdown.load(Ordering::Acquire) {
                    task_tracker.close();
                    break;
                }
            }
            _ => {}
        }
    }
    info!("Event loop shutting down. Waiting for all background tasks to complete...");
    task_tracker.wait().await;
}

/// Report errors for handler functions, and spawn them into background tasks
fn wrap_handle<F: IntoFuture<Output = Result<(), Error>> + Send + 'static>(
    rt: &tokio::runtime::Handle,
    tt: &TaskTracker,
    fut: F,
) where
    <F as IntoFuture>::IntoFuture: Send,
{
    // Tell the task tracker to track this new task
    // and create it on `rt`
    tt.spawn_on(
        // Can wait in the background
        async {
            // if the function errors, report it.
            if let Err(source) = fut.await {
                // Discord API errors are warnings, all others
                // are... errors.
                match source {
                    Error::DiscordApi(source) => warn!(?source),
                    _ => {
                        error!(?source);
                    }
                }
            }
        },
        rt,
    );
}

fn get_commands() -> Vec<Command> {
    let target_argument = CommandOption {
        autocomplete: None,
        channel_types: None,
        choices: None,
        description: "Which user to target".to_string(),
        description_localizations: None,
        kind: CommandOptionType::User,
        max_length: None,
        max_value: None,
        min_length: None,
        min_value: None,
        name: SLASH_ARG_NAME.to_string(),
        name_localizations: None,
        options: None,
        required: Some(true),
    };

    [
        CommandBuilder::new(GET_PROGRESS_NAME_CTX, "", CommandType::Message)
            .default_member_permissions(Permissions::MODERATE_MEMBERS)
            .contexts([InteractionContextType::Guild])
            .build(),
        CommandBuilder::new(GET_PROGRESS_NAME_CTX, "", CommandType::User)
            .default_member_permissions(Permissions::MODERATE_MEMBERS)
            .contexts([InteractionContextType::Guild])
            .build(),
        CommandBuilder::new(RESET_PROGRESS_NAME_CTX, "", CommandType::Message)
            .default_member_permissions(Permissions::MODERATE_MEMBERS)
            .contexts([InteractionContextType::Guild])
            .build(),
        CommandBuilder::new(RESET_PROGRESS_NAME_CTX, "", CommandType::User)
            .default_member_permissions(Permissions::MODERATE_MEMBERS)
            .contexts([InteractionContextType::Guild])
            .build(),
        CommandBuilder::new(
            GET_PROGRESS_NAME_SLASH,
            "Get a user's leveling progress.",
            CommandType::ChatInput,
        )
        .default_member_permissions(Permissions::MODERATE_MEMBERS)
        .contexts([InteractionContextType::Guild])
        .option(target_argument.clone())
        .build(),
        CommandBuilder::new(
            RESET_PROGRESS_NAME_SLASH,
            "Reset a user's leveling progress.",
            CommandType::ChatInput,
        )
        .default_member_permissions(Permissions::MODERATE_MEMBERS)
        .contexts([InteractionContextType::Guild])
        .option(target_argument)
        .build(),
    ]
    .to_vec()
}

async fn handle_interaction(ic: InteractionCreate, state: AppState) -> Result<(), Error> {
    // if we get an error, report it right in here
    let mut interaction_response_data = command(&ic, state.clone()).await.unwrap_or_else(|err| {
        InteractionResponseDataBuilder::new()
            .content(format!("Error: {err}"))
            .flags(MessageFlags::EPHEMERAL)
            .build()
    });

    // we NEVER EVER EVER want to send a public response. Ever.
    interaction_response_data.flags = Some(MessageFlags::EPHEMERAL);
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
        GET_PROGRESS_NAME_CTX | GET_PROGRESS_NAME_SLASH => get_progress(data.as_ref(), state).await,
        RESET_PROGRESS_NAME_CTX | RESET_PROGRESS_NAME_SLASH => {
            reset_progress(data.as_ref(), state).await
        }
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
        format!("<@{id}> has no activity.")
    } else {
        let active_minutes_pluralizer = if active_minutes == 1 { "" } else { "s" };
        format!(
            "User <@{id}> has been active for {} minute{} out of {} required to receive the role.",
            active_minutes, active_minutes_pluralizer, state.activity_minutes,
        )
    };

    let embed = EmbedBuilder::new().description(msg).build();
    let ird = InteractionResponseDataBuilder::new()
        .embeds([embed])
        .flags(MessageFlags::EPHEMERAL)
        .build();
    Ok(ird)
}

async fn reset_progress(
    data: &CommandData,
    state: AppState,
) -> Result<InteractionResponseData, Error> {
    let id = get_target(data)?;
    let id_i64 = id_to_db(id);
    let q = query!("DELETE FROM users WHERE id = ?1", id_i64)
        .execute(&state.db)
        .await?;
    let msg = if q.rows_affected() == 0 {
        format!("<@{id}> had no previous progress.")
    } else {
        format!("<@{id}>'s progress has been reset.")
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
        CommandType::User => data.target_id.ok_or(Error::NoTargetId)?.cast(),
        CommandType::Message => {
            let target_id = data.target_id.ok_or(Error::NoTargetId)?;
            data.resolved
                .as_ref()
                .ok_or(Error::NoResolvedData)?
                .messages
                .get(&target_id.cast())
                .ok_or(Error::NoAuthorResolvedData)?
                .author
                .id
        }
        CommandType::ChatInput => {
            let option = data
                .options
                .iter()
                .find(|v| v.name == SLASH_ARG_NAME)
                .ok_or(Error::NoChatInputOption)?;
            let CommandOptionValue::User(uid) = option.value else {
                return Err(Error::WrongChatInputOptionValue);
            };
            uid
        }
        _ => return Err(Error::UnknownCommandType),
    };
    Ok(id)
}

async fn handle_message(mc: Message, state: AppState) -> Result<(), Error> {
    let member = mc.member.ok_or(Error::NoPartialMember)?;
    // immediately ignore this message if:
    // the author is also a bot
    // the message is not in our guild (not ((it's in a guild) and (it's in ours)))
    // the user already has the role we'd be giving them
    // the user still needs to wait to send another message
    let incorrect_guild = !mc.guild_id.is_some_and(|v| v == state.guild_id);
    let has_role = member.roles.contains(&state.role_id);
    if mc.author.bot || incorrect_guild || has_role {
        debug!(
            author_id = mc.author.id.get(),
            bot = mc.author.bot,
            guild_id = mc.guild_id.map(Id::get),
            expected_guild_id = state.guild_id.get(),
            incorrect_guild,
            has_role,
            "skipped adding XP to user",
        );
        return Ok(());
    }

    let db_id = id_to_db(mc.author.id);
    let db_timestamp = snowflake_to_timestamp(mc.id);
    // insert initial values
    // if this user's values already exist
    // add 1 to active_minutes for that user
    // and return that value
    let active_minutes = query!(
        "INSERT INTO users
        (id, active_minutes, last_message)
        VALUES (?1, 1, ?2)
        ON CONFLICT DO UPDATE SET
        active_minutes = active_minutes + 1,
        last_message = ?2
        WHERE id = ?1
        AND last_message + ?3 <= ?2
        RETURNING active_minutes",
        db_id,
        db_timestamp,
        state.cooldown_seconds
    )
    .fetch_optional(&state.db)
    .await?
    .map(|v| v.active_minutes);

    let Some(active_minutes) = active_minutes else {
        debug!(id = ?mc.author.id, "Not giving role to user- on cooldown");
        return Ok(());
    };

    // if they've been active long enough, give them the role
    if active_minutes >= state.activity_minutes {
        trace!(active_minutes, user = ?mc.author.id, "adding role to user");
        state
            .http
            .add_guild_member_role(state.guild_id, mc.author.id, state.role_id)
            .await?;
    } else {
        trace!(active_minutes, user = ?mc.author.id,"skipped adding role to user"
        );
    }
    Ok(())
}

/// databases hate unsigned ints, so we cast our IDs to i64s
#[inline]
fn id_to_db<T>(id: Id<T>) -> i64 {
    #[allow(clippy::cast_possible_wrap)]
    {
        id.get() as i64
    }
}

#[derive(Clone)]
pub struct AppState {
    pub http: Arc<Client>,
    pub db: SqlitePool,
    pub guild_id: Id<GuildMarker>,
    pub role_id: Id<RoleMarker>,
    pub activity_minutes: i64,
    pub cooldown_seconds: i64,
    pub app_id: Id<ApplicationMarker>,
    pub bot_id: Id<UserMarker>,
    pub shutdown: Arc<AtomicBool>,
}

// Convert a discord message ID to a seconds value of when it was sent relative to the discord epoch
fn snowflake_to_timestamp<T>(id: Id<T>) -> i64 {
    // this is safe, because dividing an u64 by 1000 ensures it is a valid i64
    ((id.get() >> 22) / 1000).try_into().unwrap_or(0)
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("twilight-http error")]
    DiscordApi(#[from] twilight_http::Error),
    #[error("sqlx error")]
    Sqlx(#[from] sqlx::Error),
    #[error("Discord did not send the resolved data section of the interaction!")]
    NoResolvedData,
    #[error("Discord did not send a message author matching the target of the interaction!")]
    NoAuthorResolvedData,
    #[error("Discord did not send the target of the interaction!")]
    NoInteractionData,
    #[error("Discord did not send the partial member for this message!")]
    NoPartialMember,
    #[error("Discord did not send guild ID")]
    NoGuildId,
    #[error("There is no command with this name")]
    UnknownCommand,
    #[error("There is a command with this name, but not with this type")]
    UnknownCommandType,
    #[error("Discord did not send a target ID")]
    NoTargetId,
    #[error("Required argument was not found")]
    NoChatInputOption,
    #[error("Required argument was of wrong type")]
    WrongChatInputOptionValue,
}
