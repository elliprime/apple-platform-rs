// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use {
    git2::{Commit, Repository},
    std::path::{Path, PathBuf},
};

/// Canonical Git repository for PyOxidizer.
const CANONICAL_GIT_REPO_URL: &str = "https://github.com/indygreg/PyOxidizer.git";

/// Root Git commit for PyOxidizer.
const ROOT_COMMIT: &str = "b1f95017c897e0fd3ed006aec25b6886196a889d";

fn canonicalize_path(path: &Path) -> Result<PathBuf, std::io::Error> {
    let mut p = path.canonicalize()?;

    // Strip \\?\ prefix on Windows and replace \ with /, which is valid.
    if cfg!(windows) {
        let mut s = p.display().to_string().replace("\\", "/");
        if s.starts_with("//?/") {
            s = s[4..].to_string();
        }

        p = PathBuf::from(s);
    }

    Ok(p)
}

/// Find the root Git commit given a starting Git commit.
///
/// This just walks parents until it gets to a commit without any.
fn find_root_git_commit(commit: Commit) -> Commit {
    let mut current = commit;

    while current.parent_count() != 0 {
        current = current.parents().next().unwrap();
    }

    current
}

fn main() {
    let cwd = std::env::current_dir().expect("could not obtain current directory");

    // Various crates that resolve commits and versions from git shell out to `git`.
    // This isn't reliable, especially on Windows. So we use libgit2 to extract data
    // from the git repo, if present.
    let (repo_path, git_commit) = if let Ok(repo) = Repository::discover(&cwd) {
        if let Ok(head_ref) = repo.head() {
            if let Ok(commit) = head_ref.peel_to_commit() {
                let root = find_root_git_commit(commit.clone());

                if root.id().to_string() == ROOT_COMMIT {
                    let path = canonicalize_path(repo.workdir().expect("could not obtain workdir"))
                        .expect("could not canonicalize repo path");

                    (
                        Some(path.display().to_string()),
                        Some(format!("{}", commit.id())),
                    )
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            }
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    let pkg_version =
        std::env::var("CARGO_PKG_VERSION").expect("could not obtain CARGO_PKG_VERSION");

    let (pyoxidizer_version, git_tag) = if pkg_version.ends_with("-pre") {
        (
            format!(
                "{}-{}",
                pkg_version,
                git_commit.clone().unwrap_or_else(|| "UNKNOWN".to_string())
            ),
            "".to_string(),
        )
    } else {
        (pkg_version.clone(), format!("v{}", pkg_version))
    };

    println!("cargo:rustc-env=PYOXIDIZER_VERSION={}", pyoxidizer_version);

    println!(
        "cargo:rustc-env=GIT_REPO_PATH={}",
        repo_path.unwrap_or_else(|| "".to_string())
    );
    // TODO detect builds from forks via build.rs environment variable.
    println!("cargo:rustc-env=GIT_REPO_URL={}", CANONICAL_GIT_REPO_URL);
    println!("cargo:rustc-env=GIT_TAG={}", git_tag);

    println!(
        "cargo:rustc-env=GIT_COMMIT={}",
        match git_commit {
            Some(commit) => commit,
            None => "UNKNOWN".to_string(),
        }
    );

    println!(
        "cargo:rustc-env=HOST={}",
        std::env::var("HOST").expect("HOST not set")
    );
}
