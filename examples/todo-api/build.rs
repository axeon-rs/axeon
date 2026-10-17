// Rebuild when a migration is added, so `embed_migrations!` picks it up.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
