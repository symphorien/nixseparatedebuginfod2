//! Determine which file corresponds to the requested path

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

use tracing::Level;

use crate::vfs::WalkableDirectory;

/// Returns the set of files in this directory with the specified file name
///
/// Paths are returned relative to `dir`.
///
/// Errors are ignored.
fn find_file_in_dir<T: WalkableDirectory>(dir: &T, file_name: &OsStr) -> Vec<PathBuf> {
    let mut result = Vec::new();
    for file in dir.list_files_recursively() {
        match file {
            Err(e) => {
                tracing::warn!("failed to walk source {dir:?}: {:#}", e);
                continue;
            }
            Ok(f) => {
                if f.file_name() == Some(file_name) {
                    result.push(f)
                }
            }
        }
    }
    result
}

/// a number that expresses how close the candidate path is to the reference. higher is closer.
fn matching_measure(candidate: &Path, reference: &Path) -> usize {
    candidate
        .iter()
        .rev()
        .zip(reference.iter().rev())
        .position(|(ref c, ref t)| c != t)
        .unwrap_or_else(|| candidate.iter().count())
}

/// returns the path with higher matching_measure
///
/// None if `candidates` is empty
///
/// Err if there are several best matches.
fn best_matching_measure(
    candidates: &[PathBuf],
    reference: &Path,
) -> anyhow::Result<Option<PathBuf>> {
    let ranked: Vec<_> = candidates
        .iter()
        .map(|c| (matching_measure(c, reference), c))
        .collect();
    let Some(best) = ranked.iter().map(|(measure, _)| measure).max() else {
        return Ok(None);
    };
    let equals: Vec<_> = ranked
        .iter()
        .filter_map(|(measure, c)| if measure == best { Some(c) } else { None })
        .collect();
    if equals.len() != 1 {
        anyhow::bail!(
            "cannot tell {:?} apart for target {}",
            &equals,
            reference.display()
        );
    }
    Ok(Some(equals[0].to_path_buf()))
}

#[derive(Debug, Clone, Eq, PartialEq)]
/// Where the file should be taken
pub enum SourceMatch {
    /// take the file from the source
    Source(PathBuf),
    /// take the file from the overlay because it has been patched during build
    Overlay(PathBuf),
}

/// Attempts to find a file that matches the request in an existing directory of source files
///
/// Returns a path relative to `source_dir`
///
/// Returns None if no file matches
///
/// Returns Err if several file match and we don't know which one is the best one.
#[tracing::instrument(level=Level::DEBUG)]
pub fn get_file_for_source<T: WalkableDirectory>(
    source_dir: &T,
    overlay_dir: &T,
    request: &Path,
) -> anyhow::Result<Option<SourceMatch>> {
    let Some(filename) = request.file_name() else {
        anyhow::bail!("requested path {} has no filename", request.display())
    };
    let candidates = find_file_in_dir(source_dir, filename);
    let best_source = match best_matching_measure(&candidates, request) {
        Err(e) => return Err(e),
        Ok(None) => return Ok(None),
        Ok(Some(x)) => x,
    };
    let overlay_candidates = find_file_in_dir(overlay_dir, filename);
    let matching_overlay_candiates: Vec<_> = overlay_candidates
        .iter()
        .filter(|c| match best_matching_measure(&candidates, c) {
            Err(_) => false,
            Ok(None) => false,
            Ok(Some(ref f)) => f == &best_source,
        })
        .collect();
    match &matching_overlay_candiates[..] {
        [] => Ok(Some(SourceMatch::Source(best_source))),
        [best_overlay] => Ok(Some(SourceMatch::Overlay(best_overlay.into()))),
        _ => {
            tracing::warn!("several overlay files {matching_overlay_candiates:?} may correspond to source match {best_source:?}, returning source match");
            Ok(Some(SourceMatch::Source(best_source)))
        }
    }
}

#[cfg(test)]
fn make_test_source_path(paths: Vec<&'static str>) -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    for path in paths {
        let path = dir.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "content").unwrap();
    }
    dir
}

#[test]
fn get_file_for_source_simple() {
    let dir = make_test_source_path(vec!["soft-version/src/main.c", "soft-version/src/Makefile"]);
    let overlay = make_test_source_path(vec![]);
    let res = get_file_for_source(
        &dir.path(),
        &overlay.path(),
        "/source/soft-version/src/main.c".as_ref(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        res,
        SourceMatch::Source(PathBuf::from("soft-version/src/main.c"))
    );
}

#[test]
fn get_file_for_source_different_dir() {
    let dir = make_test_source_path(vec!["lib/core-net/network.c", "lib/plat/optee/network.c"]);
    let overlay = make_test_source_path(vec![]);
    let res = get_file_for_source(
        &dir.path(),
        &overlay.path(),
        "/build/source/lib/core-net/network.c".as_ref(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        res,
        SourceMatch::Source(PathBuf::from("lib/core-net/network.c"))
    );
}

#[test]
fn get_file_for_source_regression_pr_7() {
    let dir = make_test_source_path(vec![
        "store/source/lib/core-net/network.c",
        "store/source/lib/plat/optee/network.c",
    ]);
    let overlay = make_test_source_path(vec![]);
    let res = get_file_for_source(
        &dir.path(),
        &overlay.path(),
        "build/source/lib/core-net/network.c".as_ref(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        res,
        SourceMatch::Source(PathBuf::from("store/source/lib/core-net/network.c"))
    );
}

#[test]
fn get_file_for_source_no_right_filename() {
    let dir = make_test_source_path(vec![
        "store/source/lib/core-net/network.c",
        "store/source/lib/plat/optee/network.c",
    ]);
    let overlay = make_test_source_path(vec![]);
    let res = get_file_for_source(
        &dir.path(),
        &overlay.path(),
        "build/source/lib/core-net/somethingelse.c".as_ref(),
    );
    assert_eq!(res.unwrap(), None);
}

#[test]
fn get_file_for_source_glibc() {
    let dir = make_test_source_path(vec![
        "glibc-2.37/sysdeps/unix/sysv/linux/openat64.c",
        "glibc-2.37/sysdeps/mach/hurd/openat64.c",
        "glibc-2.37/io/openat64.c",
    ]);
    let overlay = make_test_source_path(vec![]);
    let res = get_file_for_source(
        &dir.path(),
        &overlay.path(),
        "/build/glibc-2.37/io/../sysdeps/unix/sysv/linux/openat64.c".as_ref(),
    );
    assert_eq!(
        res.unwrap().unwrap(),
        SourceMatch::Source(PathBuf::from(
            "glibc-2.37/sysdeps/unix/sysv/linux/openat64.c"
        ))
    );
}

#[test]
fn get_file_for_source_misleading_dir() {
    let dir = make_test_source_path(vec!["store/store/wrong/dir/file", "good/dir/store/file"]);
    let overlay = make_test_source_path(vec![]);
    let res = get_file_for_source(
        &dir.path(),
        &overlay.path(),
        "/build/project/store/file".as_ref(),
    );
    assert_eq!(
        res.unwrap().unwrap(),
        SourceMatch::Source(PathBuf::from("good/dir/store/file"))
    );
}

#[test]
fn get_file_for_source_ambiguous() {
    let sources = vec![
        "glibc-2.37/sysdeps/unix/sysv/linux/openat64.c",
        "glibc-2.37/sysdeps/mach/hurd/openat64.c",
        "glibc-2.37/io/openat64.c",
    ];
    let dir = make_test_source_path(sources.clone());
    let overlay = make_test_source_path(vec![]);
    let res = get_file_for_source(
        &dir.path(),
        &overlay.path(),
        "/build/glibc-2.37/fakeexample/openat64.c".as_ref(),
    );
    assert!(res.is_err());
    let msg = dbg!(res.unwrap_err().to_string());
    assert!(dbg!(&msg).contains("cannot tell"));
    assert!(msg.contains("apart"));
    for source in sources {
        assert!(msg.contains(source));
    }
}

#[test]
fn get_file_for_source_overlay_nothing_to_do() {
    let dir = make_test_source_path(vec!["lib/core-net/network.c", "lib/plat/optee/network.c"]);
    let overlay = make_test_source_path(vec!["lib/different"]);
    let res = get_file_for_source(
        &dir.path(),
        &overlay.path(),
        "/build/source/lib/core-net/network.c".as_ref(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        res,
        SourceMatch::Source(PathBuf::from("lib/core-net/network.c"))
    );
}

#[test]
fn get_file_for_source_overlay_easy() {
    let dir = make_test_source_path(vec!["lib/core-net/network.c", "lib/plat/optee/network.c"]);
    let overlay = make_test_source_path(vec!["source/lib/core-net/network.c"]);
    let res = get_file_for_source(
        &dir.path(),
        &overlay.path(),
        "/build/source/lib/core-net/network.c".as_ref(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        res,
        SourceMatch::Overlay(PathBuf::from("source/lib/core-net/network.c"))
    );
}

#[test]
fn get_file_for_source_overlay_other_path_patched() {
    let dir = make_test_source_path(vec!["lib/core-net/network.c", "lib/plat/optee/network.c"]);
    let overlay = make_test_source_path(vec!["source/lib/core-net/network.c"]);
    let res = get_file_for_source(
        &dir.path(),
        &overlay.path(),
        "/build/source/lib/plat/optee/network.c".as_ref(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        res,
        SourceMatch::Source(PathBuf::from("lib/plat/optee/network.c"))
    );
}

#[test]
fn get_file_for_source_overlay_choice() {
    let dir = make_test_source_path(vec!["lib/core-net/network.c", "lib/plat/optee/network.c"]);
    let overlay = make_test_source_path(vec![
        "source/lib/core-net/network.c",
        "source/lib/plat/optee/network.c",
    ]);
    let res = get_file_for_source(
        &dir.path(),
        &overlay.path(),
        "/build/source/lib/plat/optee/network.c".as_ref(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        res,
        SourceMatch::Overlay(PathBuf::from("source/lib/plat/optee/network.c"))
    );
}
