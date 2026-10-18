//! Runs the `axeon-orm` binary against a SQLite file.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("axeon-orm-cli-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn cli(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_axeon-orm"))
        .args(args)
        .current_dir(dir)
        .env_remove("DATABASE_URL")
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn migrations(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir.join("migrations"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn new_run_and_status() {
    let tmp = TempDir::new("flow");
    let dir = tmp.0.as_path();
    let url = format!("--database-url=sqlite://{}/app.db", dir.display());

    // new: timestamped, slugged, always newer than the last one.
    let created = cli(dir, &["migrate", "new", "Create", "users"]);
    assert!(created.status.success(), "{}", stderr(&created));
    assert!(cli(dir, &["migrate", "new", "add-posts"]).status.success());
    let files = migrations(dir);
    assert_eq!(files.len(), 2);
    assert!(files[0].ends_with("_create_users.sql") && files[1].ends_with("_add_posts.sql"), "{:?}", files);
    let version = |f: &str| f.split('_').next().unwrap().parse::<i64>().unwrap();
    assert!(version(&files[1]) > version(&files[0]));
    assert!(version(&files[0]) > 20_260_000_000_000, "a UTC timestamp");

    let path = |i: usize| dir.join("migrations").join(&files[i]);
    std::fs::write(path(0), "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);").unwrap();
    std::fs::write(path(1), "CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL);").unwrap();

    let status = cli(dir, &["migrate", "status", &url]);
    assert!(status.status.success(), "{}", stderr(&status));
    assert!(stdout(&status).contains("0 applied, 2 pending."), "{}", stdout(&status));

    let run = cli(dir, &["migrate", "run", &url]);
    assert!(run.status.success(), "{}", stderr(&run));
    assert!(stdout(&run).contains(&format!("Applied {}", files[0].trim_end_matches(".sql"))), "{}", stdout(&run));
    assert!(stdout(&run).contains("_add_posts"));

    let again = cli(dir, &["migrate", "run", &url]);
    assert!(stdout(&again).contains("Nothing to apply"), "{}", stdout(&again));
    let status = cli(dir, &["migrate", "status", &url]);
    assert!(stdout(&status).contains("2 applied, 0 pending."), "{}", stdout(&status));
    assert!(stdout(&status).lines().filter(|l| l.starts_with("Applied")).count() == 2);

    // Editing an applied migration is reported and fails the command.
    std::fs::write(path(1), "CREATE TABLE posts (id INTEGER PRIMARY KEY);").unwrap();
    let status = cli(dir, &["migrate", "status", &url]);
    assert_eq!(status.status.code(), Some(1));
    assert!(stdout(&status).contains("CHANGED"), "{}", stdout(&status));
    let run = cli(dir, &["migrate", "run", &url]);
    assert_eq!(run.status.code(), Some(1));
    assert!(stderr(&run).contains("was changed after it was applied"), "{}", stderr(&run));

    // A deleted file is reported too.
    std::fs::remove_file(path(1)).unwrap();
    let status = cli(dir, &["migrate", "status", &url]);
    assert!(stdout(&status).contains("file missing"), "{}", stdout(&status));
}

#[test]
fn reads_database_url_from_dotenv() {
    let tmp = TempDir::new("dotenv");
    let dir = tmp.0.as_path();
    std::fs::write(dir.join(".env"), format!("# settings\nOTHER=1\nDATABASE_URL=\"sqlite://{}/env.db\"\n", dir.display())).unwrap();
    assert!(cli(dir, &["migrate", "new", "init"]).status.success());
    let file = dir.join("migrations").join(&migrations(dir)[0]);
    std::fs::write(file, "CREATE TABLE t (id INTEGER PRIMARY KEY);").unwrap();
    let run = cli(dir, &["migrate", "run"]);
    assert!(run.status.success(), "{}", stderr(&run));
    assert!(dir.join("env.db").exists());
}

#[test]
fn explains_mistakes() {
    let tmp = TempDir::new("errors");
    let dir = tmp.0.as_path();

    let no_url = cli(dir, &["migrate", "status"]);
    assert_eq!(no_url.status.code(), Some(1));
    assert!(stderr(&no_url).contains("DATABASE_URL"), "{}", stderr(&no_url));

    let unknown = cli(dir, &["migrate", "explode"]);
    assert_eq!(unknown.status.code(), Some(2));
    assert!(stderr(&unknown).contains("Usage:"));
    assert_eq!(cli(dir, &["--nope"]).status.code(), Some(2));
    assert_eq!(cli(dir, &["migrate", "new", "--", "!!"]).status.code(), Some(2));

    let help = cli(dir, &["--help"]);
    assert!(help.status.success() && stdout(&help).contains("migrate new"));
    let version = cli(dir, &["--version"]);
    assert!(stdout(&version).starts_with("axeon-orm 0."));

    let no_folder = cli(dir, &["migrate", "run", "--database-url", "sqlite::memory:"]);
    assert_eq!(no_folder.status.code(), Some(1));
    assert!(stderr(&no_folder).contains("no migrations folder"), "{}", stderr(&no_folder));

    let bad_url = cli(dir, &["migrate", "status", "--database-url", "oracle://x"]);
    assert_eq!(bad_url.status.code(), Some(1));
    assert!(stderr(&bad_url).contains("unsupported database URL"), "{}", stderr(&bad_url));
}
