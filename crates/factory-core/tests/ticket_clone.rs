//! A ticket's clone is a complete repository on its own: a container that
//! mounts only the clone's folder can run every git command in it.

mod support;

use factory_core::Git;
use support::{World, git, have_git, write};

#[tokio::test]
async fn a_ticket_clone_stands_alone_and_pushes_to_the_real_remote() {
    if !have_git() {
        eprintln!("git not available; skipping");
        return;
    }
    let world = World::new();
    let repo = Git::new(&world.factory);
    repo.fetch_prune("origin").await.expect("fetch");
    let path = world.home.work_dir("01TICKET");
    write(&path.join("stale.txt"), "from an earlier run\n");

    let clone = repo
        .clone_for_ticket(&path, "origin", "factory/01TICKET", "origin/main")
        .await
        .expect("clone");

    assert!(
        !path.join("stale.txt").exists(),
        "a stale clone is replaced"
    );
    assert!(
        path.join(".git").is_dir(),
        "`.git` is a folder, not a gitlink"
    );
    assert!(
        !path.join(".git/objects/info/alternates").exists(),
        "objects do not point back into the factory's repository"
    );
    assert_eq!(
        git(&path, &["remote", "get-url", "origin"]),
        world.remote.to_string_lossy(),
        "origin is the real remote, not the factory's checkout"
    );
    assert_eq!(
        git(&path, &["rev-parse", "HEAD"]),
        git(&world.factory, &["rev-parse", "origin/main"]),
    );
    assert_eq!(
        git(&path, &["branch", "--show-current"]),
        "factory/01TICKET"
    );

    // Moving the factory's repository away proves the clone needs nothing
    // from it, which is what a container mounting only the clone sees.
    let moved = world.dir.path().join("moved-away");
    std::fs::rename(&world.factory, &moved).expect("move");
    git(&path, &["status", "--porcelain"]);
    assert!(git(&path, &["grep", "-n", "repo", "--", "README.md"]).contains("README.md:1"));
    assert_eq!(
        clone
            .diff_names("origin/main", "HEAD")
            .await
            .expect("diff")
            .len(),
        0
    );

    write(&path.join("change.txt"), "work\n");
    assert!(clone.commit_all("factory: work").await.expect("commit"));
    clone
        .push("origin", "factory/01TICKET")
        .await
        .expect("push to the real remote");
    git(
        &world.remote,
        &["rev-parse", "--verify", "factory/01TICKET"],
    );

    factory_core::git::remove_clone(&path).expect("remove");
    assert!(!path.exists());
    factory_core::git::remove_clone(&path).expect("removing nothing is fine");
}
