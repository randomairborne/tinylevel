# tinylevel

A stupidly simple role-granting discord bot.

Stores data in sqlite, only needs a discord token and other basic config.

environment variables:

```dotenv
DISCORD_TOKEN=<your bot token>
ROLE_ID=<your activity role id>
GUILD_ID=<your guild id>
ACTIVITY_MINUTES=60
COOLDOWN_SECONDS=60
```

run with:

```shell
curl -o compose.yaml https://raw.githubusercontent.com/randomairborne/tinylevel/main/compose.yaml
docker compose up -d
```