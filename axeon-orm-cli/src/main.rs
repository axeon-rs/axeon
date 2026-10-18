//! `axeon-orm`: create, run and list axeon-orm migrations.
//!
//! ```text
//! axeon-orm migrate new <name>     Create migrations/<timestamp>_<name>.sql
//! axeon-orm migrate run            Apply pending migrations
//! axeon-orm migrate status         List applied and pending migrations
//! ```

use axeon_orm::migrate::Migrator;
use axeon_orm::Db;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
axeon-orm: migrations for axeon-orm

Usage:
  axeon-orm migrate new <name>     Create <dir>/<timestamp>_<name>.sql
  axeon-orm migrate run            Apply pending migrations
  axeon-orm migrate status         List applied and pending migrations

Options:
  --database-url <URL>   Database to use. Default: $DATABASE_URL, also read
                         from a .env file in the current directory.
  --dir <DIR>            Migrations folder. Default: migrations
  -h, --help             Show this help
  -V, --version          Show the version

URLs: postgres://user:pass@host/db, mysql://user:pass@host/db,
sqlite://app.db (created if missing).";

/// Parsed command line.
struct Args {
    command: Vec<String>,
    database_url: Option<String>,
    dir: PathBuf,
}

/// A failure with its exit code: 1 for errors, 2 for bad usage.
struct Failure(u8, String);

impl<E: std::fmt::Display> From<E> for Failure {
    fn from(error: E) -> Self {
        Failure(1, error.to_string())
    }
}

fn parse(mut raw: impl Iterator<Item = String>) -> Result<Option<Args>, Failure> {
    let mut args = Args { command: Vec::new(), database_url: None, dir: PathBuf::from("migrations") };
    while let Some(arg) = raw.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "-V" | "--version" => {
                println!("axeon-orm {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--database-url" => {
                args.database_url = Some(raw.next().ok_or_else(|| Failure(2, "--database-url needs a value".into()))?)
            }
            "--dir" => args.dir = PathBuf::from(raw.next().ok_or_else(|| Failure(2, "--dir needs a value".into()))?),
            flag if flag.starts_with("--database-url=") => args.database_url = Some(flag["--database-url=".len()..].to_string()),
            flag if flag.starts_with("--dir=") => args.dir = PathBuf::from(&flag["--dir=".len()..]),
            flag if flag.starts_with('-') => return Err(Failure(2, format!("unknown option `{}`\n\n{}", flag, USAGE))),
            _ => args.command.push(arg),
        }
    }
    Ok(Some(args))
}

/// `DATABASE_URL` from the environment, or else from `./.env`.
fn database_url(args: &Args) -> Result<String, Failure> {
    if let Some(url) = &args.database_url {
        return Ok(url.clone());
    }
    if let Ok(url) = std::env::var("DATABASE_URL") {
        return Ok(url);
    }
    if let Ok(contents) = std::fs::read_to_string(".env") {
        for line in contents.lines() {
            let line = line.trim().trim_start_matches("export ").trim();
            if let Some(value) = line.strip_prefix("DATABASE_URL=") {
                let value = value.trim();
                let unquoted = value
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                    .unwrap_or(value);
                return Ok(unquoted.to_string());
            }
        }
    }
    Err(Failure(1, "no database URL: set DATABASE_URL (or put it in .env), or pass --database-url".into()))
}

/// Lowercase words joined by `_`: `Add Users-Table` becomes `add_users_table`.
fn slug(name: &str) -> String {
    let words: Vec<String> = name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    words.join("_")
}

/// Creates `<dir>/<version>_<name>.sql`, with a version newer than every
/// existing migration.
fn new_migration(dir: &Path, name: &str) -> Result<PathBuf, Failure> {
    let name = slug(name);
    if name.is_empty() {
        return Err(Failure(2, "give the migration a name, e.g. `axeon-orm migrate new create_users`".into()));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {}", dir.display(), e))?;
    let latest = load(dir, false)?.migrations().last().map(|m| m.version);
    let now: i64 = chrono::Utc::now().format("%Y%m%d%H%M%S").to_string().parse()?;
    let version = latest.map_or(now, |latest| now.max(latest + 1));
    let path = dir.join(format!("{}_{}.sql", version, name));
    let template = format!(
        "-- {}\n--\n-- Write the SQL for this change. Once a migration has been applied it\n-- can't be edited: add a new migration instead.\n\n",
        name.replace('_', " ")
    );
    std::fs::write(&path, template).map_err(|e| format!("cannot write {}: {}", path.display(), e))?;
    Ok(path)
}

/// The migrations in `dir`. A missing folder has none, unless `required`.
fn load(dir: &Path, required: bool) -> Result<Migrator, Failure> {
    match (dir.is_dir(), required) {
        (true, _) => Ok(Migrator::from_dir(dir)?),
        (false, false) => Ok(Migrator::new(Vec::new())),
        (false, true) => Err(Failure(
            1,
            format!("no migrations folder at {}; create a migration with `axeon-orm migrate new <name>`", dir.display()),
        )),
    }
}

async fn run(args: Args) -> Result<(), Failure> {
    let words: Vec<&str> = args.command.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["migrate", "new", name @ ..] if !name.is_empty() => {
            let path = new_migration(&args.dir, &name.join(" "))?;
            println!("Created {}", path.display());
        }
        ["migrate", "run"] => {
            let migrator = load(&args.dir, true)?;
            let db = Db::connect(&database_url(&args)?).await?;
            let applied = migrator.run(&db).await?;
            if applied.is_empty() {
                println!("Nothing to apply: the database is up to date.");
            }
            for version in &applied {
                let name = migrator.migrations().iter().find(|m| m.version == *version).map(|m| m.name.as_ref()).unwrap_or("");
                println!("Applied {}_{}", version, name);
            }
            db.close().await;
        }
        ["migrate", "status"] => {
            let migrator = load(&args.dir, false)?;
            let db = Db::connect(&database_url(&args)?).await?;
            let status = migrator.status(&db).await?;
            db.close().await;
            if status.is_empty() {
                println!("No migrations in {}.", args.dir.display());
            }
            let mut problems = 0;
            for migration in &status {
                let state = match &migration.applied_at {
                    Some(_) if migration.missing => "Applied, file missing",
                    Some(_) if migration.changed => "Applied, CHANGED since",
                    Some(_) => "Applied",
                    None => "Pending",
                };
                problems += usize::from(migration.missing || migration.changed);
                let when = migration.applied_at.as_deref().map(|at| format!("  ({})", at)).unwrap_or_default();
                println!("{:<22}  {}_{}{}", state, migration.version, migration.name, when);
            }
            let pending = status.iter().filter(|m| m.applied_at.is_none()).count();
            println!("\n{} applied, {} pending.", status.len() - pending, pending);
            if problems > 0 {
                return Err(Failure(
                    1,
                    format!("{} migration(s) changed or missing since they were applied; add new migrations instead", problems),
                ));
            }
        }
        _ => return Err(Failure(2, USAGE.to_string())),
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let result = match parse(std::env::args().skip(1)) {
        Ok(Some(args)) => run(args).await,
        Ok(None) => {
            println!("{}", USAGE);
            Ok(())
        }
        Err(failure) => Err(failure),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure(code, message)) => {
            eprintln!("error: {}", message);
            ExitCode::from(code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::slug;

    #[test]
    fn slugs() {
        assert_eq!(slug("create_users"), "create_users");
        assert_eq!(slug("Add Users-Table"), "add_users_table");
        assert_eq!(slug("  --  "), "");
    }
}
