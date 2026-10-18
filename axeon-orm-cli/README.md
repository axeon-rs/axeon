# axeon-orm-cli

The `axeon-orm` command: create, apply and list
[axeon-orm](https://docs.rs/axeon-orm) migrations on PostgreSQL, MySQL or
SQLite.

```bash
cargo install axeon-orm-cli
```

```text
axeon-orm migrate new <name>     Create migrations/<timestamp>_<name>.sql
axeon-orm migrate run            Apply pending migrations
axeon-orm migrate status         List applied and pending migrations

Options:
  --database-url <URL>   Default: $DATABASE_URL, also read from ./.env
  --dir <DIR>            Default: migrations
```

`status` exits with code 1 when an applied migration was edited or its file
deleted, so it can guard a CI pipeline. The migrations are the same files
`axeon_orm::embed_migrations!()` and `Migrator::from_dir` use, so you can
apply them from the command line, at application startup, or both.

## License

MIT
