//! The guest side of SPEC.md's Paths section: where `ADD`, `SHARE`, and
//! `REPO` land in the guest, and which destinations can't work.

use agentfile::{Error, GuestPath, parse};

/// The name of the directory that holds the Agentfile in these checks.
const DIRECTORY: &str = "project";

/// Reads the guest paths of `lines`, after an `AGENT` line.
fn read(lines: &str) -> Result<Vec<GuestPath>, Vec<Error>> {
    let instructions = parse(&format!("AGENT claude\n{lines}")).unwrap();
    GuestPath::read(&instructions, DIRECTORY)
}

/// The guest paths of `lines`, as `(line, path)`.
fn paths(lines: &str) -> Vec<(usize, String)> {
    read(lines)
        .unwrap()
        .into_iter()
        .map(|guest| (guest.line, guest.path))
        .collect()
}

/// The one guest path of `line`.
fn path_of(line: &str) -> String {
    let mut found = paths(line);
    assert_eq!(found.len(), 1, "{line:?}: {found:?}");
    found.remove(0).1
}

/// The errors of `lines`, as `(line, message)`.
fn errors(lines: &str) -> Vec<(usize, String)> {
    read(lines)
        .unwrap_err()
        .into_iter()
        .map(|error| (error.line, error.message))
        .collect()
}

/// The one error of `line`, which must be on line 2.
fn message_of(line: &str) -> String {
    let mut found = errors(line);
    assert_eq!(found.len(), 1, "{line:?}: {found:?}");
    let (at, message) = found.remove(0);
    assert_eq!(at, 2, "{line:?}: {message}");
    message
}

fn owned(found: &[(usize, &str)]) -> Vec<(usize, String)> {
    found
        .iter()
        .map(|(line, path)| (*line, path.to_string()))
        .collect()
}

// spec(Paths): Guest paths appear as destinations.
#[test]
fn add_share_and_repo_each_get_a_guest_path() {
    assert_eq!(
        paths(
            "ADD ./prompts /opt/prompts\n\
             SHARE ./data /data\n\
             REPO https://codeberg.org/forgejo/forgejo ~/src/forgejo\n\
             REPO torvalds/linux\n\
             PLUGIN anthropics/claude-plugins-official\n\
             WORKDIR ~/src\n"
        ),
        owned(&[
            (2, "/opt/prompts"),
            (3, "/data"),
            (4, "/home/agent/src/forgejo"),
            (5, "/home/agent/git/linux"),
        ])
    );
}

#[test]
fn an_agentfile_without_destinations_has_no_guest_paths() {
    assert_eq!(paths("MODEL opus\n"), Vec::new());
}

// spec(Paths): `~/` and relative guest paths resolve under `/home/agent`, the agent's home directory.
#[test]
fn home_and_relative_guest_paths_resolve_under_home_agent() {
    assert_eq!(path_of("ADD ./a ~/out"), "/home/agent/out");
    assert_eq!(path_of("ADD ./a out"), "/home/agent/out");
    assert_eq!(path_of("ADD ./a ./out"), "/home/agent/out");
    assert_eq!(path_of("SHARE ./a out/data ro"), "/home/agent/out/data");
    assert_eq!(
        path_of("REPO torvalds/linux src/linux"),
        "/home/agent/src/linux"
    );
    assert_eq!(path_of("ADD ./a /srv/out"), "/srv/out");
}

#[test]
fn a_tilde_that_does_not_start_a_home_path_is_a_relative_name() {
    assert_eq!(path_of("ADD ./a ~out"), "/home/agent/~out");
}

// spec(Paths): Guest paths are normalized before any check.
#[test]
fn guest_paths_are_normalized() {
    assert_eq!(path_of("ADD ./a ~/x/./y//z/"), "/home/agent/x/y/z");
    assert_eq!(path_of("ADD ./a ~/x/../y"), "/home/agent/y");
    assert_eq!(path_of("ADD ./a /../../srv"), "/srv");
    assert_eq!(path_of("ADD ./a out/../in"), "/home/agent/in");
}

// spec(Paths): Guest paths are normalized before any check.
#[test]
fn normalizing_comes_before_the_reserved_check() {
    let message = message_of("ADD ./a ~/x/../.claude");
    assert!(message.contains("/home/agent/.claude"), "{message}");
    let message = message_of("ADD ./a /srv/../etc/app");
    assert!(message.contains("/etc"), "{message}");
}

// spec(Paths): Guest paths are normalized before any check.
#[test]
fn normalizing_comes_before_the_collision_check() {
    assert_eq!(
        errors("ADD ./a ~/out\nSHARE ./b /home/agent/./out/\n").len(),
        1
    );
    assert_eq!(errors("ADD ./a ~/x/../out\nADD ./b out/data\n").len(), 1);
}

// spec(Paths): When `ADD` or `SHARE` has no destination, the guest path follows one **mirror rule**.
// spec(Paths): | `./data` | `~/data` |
#[test]
fn a_relative_directory_mirrors_under_home() {
    assert_eq!(path_of("ADD ./data"), "/home/agent/data");
    assert_eq!(path_of("SHARE ./data"), "/home/agent/data");
    assert_eq!(path_of("SHARE ./data rw"), "/home/agent/data");
    assert_eq!(path_of("SHARE data ro"), "/home/agent/data");
}

// spec(Paths): | `./fixtures/db.sqlite` | `~/fixtures/db.sqlite` |
#[test]
fn a_relative_file_mirrors_under_home() {
    assert_eq!(
        path_of("ADD ./fixtures/db.sqlite"),
        "/home/agent/fixtures/db.sqlite"
    );
    assert_eq!(
        path_of("SHARE ./fixtures/./db.sqlite"),
        "/home/agent/fixtures/db.sqlite"
    );
}

// spec(Paths): | `.` | `~/<name of the Agentfile's directory>` |
#[test]
fn the_build_context_mirrors_to_its_name() {
    assert_eq!(path_of("SHARE ."), "/home/agent/project");
    assert_eq!(path_of("ADD ./"), "/home/agent/project");
    assert_eq!(path_of("SHARE ./data/.. ro"), "/home/agent/project");
}

// spec(Paths): | `~/notes` | `~/notes` |
#[test]
fn a_host_home_path_mirrors_under_the_guest_home() {
    assert_eq!(path_of("SHARE ~/notes ro"), "/home/agent/notes");
    assert_eq!(
        path_of("ADD ~/notes/today.md"),
        "/home/agent/notes/today.md"
    );
}

// spec(Paths): | `/var/log/output.log` | `/var/log/output.log` |
#[test]
fn an_absolute_host_path_mirrors_to_itself() {
    assert_eq!(
        path_of("SHARE /var/log/output.log rw"),
        "/var/log/output.log"
    );
    assert_eq!(path_of("ADD /srv//data/"), "/srv/data");
}

// spec(Paths): | `../shared` | error: give an explicit destination |
#[test]
fn a_host_path_outside_the_build_context_needs_a_destination() {
    for line in [
        "SHARE ../shared",
        "ADD ../shared",
        "SHARE ./a/../../shared ro",
        "ADD ..",
    ] {
        let message = message_of(line);
        assert!(
            message.contains("Give an explicit destination"),
            "{line}: {message}"
        );
    }
    assert_eq!(path_of("SHARE ../shared /shared"), "/shared");
}

// spec(Paths): Two destinations where one equals or contains the other are an error, whichever instructions they come from.
#[test]
fn equal_destinations_are_an_error() {
    let found = errors("ADD ./a /srv/data\nADD ./b /srv/data\n");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, 3);
    assert!(found[0].1.contains("/srv/data"), "{}", found[0].1);
    assert!(found[0].1.contains("line 2"), "{}", found[0].1);
}

// spec(Paths): Two destinations where one equals or contains the other are an error, whichever instructions they come from.
#[test]
fn a_destination_that_contains_another_is_an_error() {
    let found = errors("ADD ./a ~/out/data\nSHARE ./b ~/out\n");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, 3);
    assert!(found[0].1.contains("line 2"), "{}", found[0].1);
    let found = errors("SHARE ./b ~/out\nADD ./a ~/out/data\n");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, 3);
}

// spec(Paths): Two destinations where one equals or contains the other are an error, whichever instructions they come from.
#[test]
fn a_collision_is_found_between_any_two_instructions() {
    for lines in [
        "ADD ./data\nADD ./data /home/agent/data\n",
        "SHARE ./data\nSHARE ./other ~/data ro\n",
        "ADD ./data\nSHARE ./data\n",
        "SHARE ./src\nREPO torvalds/linux ~/src/linux\n",
        "REPO torvalds/linux ~/src\nADD ./src/main.rs\n",
        "REPO torvalds/linux\nREPO jgabor/agent-plus ~/git/linux/vendor\n",
    ] {
        let found = errors(lines);
        assert_eq!(found.len(), 1, "{lines:?}: {found:?}");
        assert_eq!(found[0].0, 3, "{lines:?}: {found:?}");
    }
}

// spec(Paths): For example, `ADD ./git` collides with `REPO`'s `~/git/<name>`.
#[test]
fn adding_git_collides_with_a_repository() {
    let found = errors("ADD ./git\nREPO torvalds/linux\n");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, 3);
    assert!(
        found[0].1.contains("/home/agent/git/linux"),
        "{}",
        found[0].1
    );
    assert!(found[0].1.contains("/home/agent/git"), "{}", found[0].1);
}

#[test]
fn siblings_with_a_common_prefix_do_not_collide() {
    assert_eq!(
        paths("ADD ./app\nSHARE ./apple\nADD ./a /srv/app\nADD ./b /srv/app-data\n"),
        owned(&[
            (2, "/home/agent/app"),
            (3, "/home/agent/apple"),
            (4, "/srv/app"),
            (5, "/srv/app-data"),
        ])
    );
}

#[test]
fn each_destination_reports_one_collision() {
    let found = errors("SHARE ./a ~/a\nSHARE ./b ~/a\nSHARE ./c ~/a\n");
    assert_eq!(
        found.iter().map(|(line, _)| *line).collect::<Vec<_>>(),
        vec![3, 4]
    );
}

#[test]
fn a_local_repository_without_a_destination_is_left_to_the_build() {
    assert_eq!(
        paths("REPO ./vendor/tool\nADD ./git\n"),
        owned(&[(3, "/home/agent/git")])
    );
    assert_eq!(path_of("REPO ./vendor/tool ~/tool"), "/home/agent/tool");
}

// spec(Paths): These guest paths are reserved.
// spec(Paths): A destination that equals, contains, or is inside one of them is an error.
// spec(Paths): System directories: `/bin`, `/boot`, `/dev`, `/etc`, `/lib*`, `/proc`, `/run`, `/sbin`, `/sys`, `/usr`
#[test]
fn system_directories_are_reserved() {
    for line in [
        "ADD ./a /bin",
        "ADD ./a /boot/grub",
        "SHARE ./a /dev",
        "SHARE ./a /etc/app ro",
        "ADD ./a /lib",
        "ADD ./a /lib64/x",
        "ADD ./a /libexec",
        "ADD ./a /proc",
        "ADD ./a /run/app",
        "ADD ./a /sbin",
        "ADD ./a /sys",
        "REPO torvalds/linux /usr/src/linux",
        "SHARE /etc/hosts",
        "SHARE /usr/local/share/data ro",
        "ADD ./a /",
    ] {
        let message = message_of(line);
        assert!(message.contains("reserved"), "{line}: {message}");
    }
}

#[test]
fn paths_beside_system_directories_are_free() {
    for line in [
        "ADD ./a /srv",
        "ADD ./a /opt/bin",
        "ADD ./a /var/run",
        "ADD ./a /binaries",
        "ADD ./a /home/agent/etc",
        "ADD ./a /li",
    ] {
        read(line).unwrap();
    }
}

// spec(Paths): These guest paths are reserved.
// spec(Paths): A destination that equals, contains, or is inside one of them is an error.
// spec(Paths): Harness configuration directories, such as `~/.claude` and `~/.codex`.
#[test]
fn harness_configuration_directories_are_reserved() {
    for line in [
        "ADD ./a ~/.claude",
        "ADD ./a ~/.claude/skills/x",
        "SHARE ./a /home/agent/.codex ro",
        "ADD ./.claude",
        "SHARE ~/.codex",
        "REPO anthropics/skills ~/.claude/skills",
        "ADD ./a ~",
        "ADD ./a /home",
        "SHARE ~ ro",
    ] {
        let message = message_of(line);
        assert!(message.contains("reserved"), "{line}: {message}");
    }
}

#[test]
fn a_reserved_harness_directory_points_to_the_instructions_that_configure_it() {
    let message = message_of("ADD ./a ~/.claude/agents");
    assert!(message.contains("`PLUGIN`"), "{message}");
}

#[test]
fn paths_beside_harness_directories_are_free() {
    for line in [
        "ADD ./a ~/.claude-notes",
        "ADD ./a ~/.config",
        "ADD ./a ~/claude",
    ] {
        read(line).unwrap();
    }
}

#[test]
fn every_error_is_reported_in_line_order() {
    assert_eq!(
        errors("SHARE ./a /etc\nADD ../b\nADD ./c ~/x\nADD ./d ~/x\n")
            .iter()
            .map(|(line, _)| *line)
            .collect::<Vec<_>>(),
        vec![2, 3, 5]
    );
}

#[test]
fn a_destination_with_an_error_is_left_out_of_the_collision_check() {
    assert_eq!(errors("ADD ./a /etc\nADD ./b /etc/app\n").len(), 2);
    assert_eq!(errors("ADD ../a\nADD ./b ~/a\n").len(), 1);
}
