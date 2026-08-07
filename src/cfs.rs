use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::Result;

/// UAPI Configuration File Specification implementation.
///
/// This follows the systemd-style configuration hierarchy where:
/// - Files in `/usr/share` are vendor defaults
/// - Files in `/run` are runtime overrides
/// - Files in `/etc` are local administrator overrides
///
/// Main files override in priority order (etc > run > usr/share).
/// Drop-in files (*.d/) are merged lexicographically, with empty files masking earlier entries.
///
/// When the recursive mode is enabled (disabled by default), the drop-in
/// directories are traversed in depth.  Every file found is subject to the
/// same rules, using the path relative to the drop-in directory as the
/// identity of the entry.
///
/// The searched prefixes are configurable, as not every kind of resource
/// belongs in `usr/share`.  Executables, for example, are better searched in
/// `usr/libexec`, `run/lib` and `var/lib`.
#[derive(Debug)]
pub struct UAPICFS {
    name: String,
    root: PathBuf,
    prefixes: Vec<PathBuf>,
    recursive: bool,
}

/// Default search prefixes for configuration files, from the lowest to the
/// highest priority.
pub const SEARCH_PREFIXES: &[&str] = &["usr/share", "run", "etc"];

/// The rung of a search ladder that a file sits on, and the rest of its path.
///
/// `path` is under `root`, and `prefixes` is a ladder like [`SEARCH_PREFIXES`],
/// from the lowest priority to the highest.  What comes back is the index of
/// the prefix that holds the file, and everything of the path below it.
///
/// That remainder is the whole point: it is the same in every prefix of the
/// ladder, so it is what turns a file in one prefix into the name of the file
/// that would shadow it in another.  The highest prefix that holds the path
/// wins, so a ladder whose prefixes nest answers with the more specific one.
///
/// Nothing is read, so this answers for a file that is not there.
pub fn rung(root: &Path, path: &Path, prefixes: &[&str]) -> Option<(usize, PathBuf)> {
    let relative = path.strip_prefix(root).ok()?;

    prefixes
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, prefix)| {
            Some((index, relative.strip_prefix(prefix).ok()?.to_path_buf()))
        })
}

/// Maximum drop-in directory depth traversed in recursive mode.  Links are
/// followed, and a loop among them is recognised as one and left alone, so this
/// bounds how deep a tree of real directories is read and nothing else.
const MAX_DEPTH: usize = 64;

impl UAPICFS {
    /// Create a new UAPI CFS parser with the default root `/`.
    pub fn new(name: &str) -> Self {
        Self::with_root(name, "/")
    }

    /// Create a new UAPI CFS parser with a custom root directory.
    pub fn with_root(name: &str, root: impl AsRef<Path>) -> Self {
        Self {
            name: name.to_string(),
            root: root.as_ref().to_path_buf(),
            prefixes: SEARCH_PREFIXES.iter().map(PathBuf::from).collect(),
            recursive: false,
        }
    }

    /// Replace the search prefixes, that default to [`SEARCH_PREFIXES`].
    ///
    /// The prefixes are relative to the root, and listed from the lowest to
    /// the highest priority, so the last one wins.  A leading `/` is ignored,
    /// so both `usr/libexec` and `/usr/libexec` refer to the same directory
    /// under the root.
    pub fn prefixes(mut self, prefixes: impl IntoIterator<Item = impl AsRef<Path>>) -> Self {
        self.prefixes = prefixes
            .into_iter()
            .map(|prefix| {
                let prefix = prefix.as_ref();
                prefix.strip_prefix("/").unwrap_or(prefix).to_path_buf()
            })
            .collect();
        self
    }

    /// Enable or disable the recursive traversal of the drop-in directories.
    ///
    /// Disabled by default.  When enabled, `<prefix>/<name>.d` is descended
    /// into instead of scanned a single level deep.  Files are identified by
    /// their path relative to `<name>.d`, so overriding and masking work
    /// across prefixes exactly like they do for top level drop-ins.
    pub fn recursive(mut self, recursive: bool) -> Self {
        self.recursive = recursive;
        self
    }

    /// Resolve all configuration files according to UAPI CFS rules.
    ///
    /// Returns files in order: main file first, then drop-ins in
    /// lexicographical order.  In recursive mode the order is lexicographical
    /// by relative path, which places a file before the contents of its own
    /// `.d` directory.
    ///
    /// Empty files (0 bytes) mask earlier files with the same relative path.
    pub fn files(&self) -> Result<Vec<PathBuf>> {
        Ok(self.entries()?.into_iter().map(|(_, path)| path).collect())
    }

    /// Resolve all configuration files, like [`Self::files`], but paired with
    /// the relative path that identifies them.
    ///
    /// The key of the main file is empty, and the key of a drop-in is its path
    /// relative to `<name>.d`, which is the identity used to override and mask
    /// entries across prefixes.
    pub fn entries(&self) -> Result<Vec<(PathBuf, PathBuf)>> {
        let mut main_file = None;
        let mut dropins = BTreeMap::new();

        for prefix in &self.prefixes {
            let path = self.root.join(prefix).join(&self.name);
            if let Ok(metadata) = fs::metadata(&path)
                && metadata.is_file()
            {
                main_file = if metadata.len() == 0 {
                    None
                } else {
                    Some(path.clone())
                };
            }

            let dropin_dir = path.with_added_extension("d");
            dropins.extend(self.collect(&dropin_dir));
        }

        let mut result = Vec::with_capacity(1 + dropins.len());
        if let Some(main) = main_file {
            result.push((PathBuf::new(), main));
        }
        result.extend(
            dropins
                .into_iter()
                .filter_map(|(key, path)| Some((key, path?))),
        );
        Ok(result)
    }

    /// Collect the regular files of `dir`, keyed by their path relative to it.
    ///
    /// A masked entry (an empty file) is reported as `None`, so that the caller
    /// can apply it over the entries collected from the previous prefixes.
    /// Subdirectories are only descended into when the recursive mode is
    /// enabled.
    ///
    /// A directory that cannot be read is passed over rather than reported: a
    /// prefix that is not there at all is the ordinary case, and one that is
    /// unreadable says the same thing to a tool that only reads.
    fn collect(&self, dir: &Path) -> BTreeMap<PathBuf, Option<PathBuf>> {
        WalkDir::new(dir)
            .max_depth(if self.recursive { MAX_DEPTH } else { 1 })
            // What a link points at, so that a drop-in kept somewhere else and
            // linked into place is read like any other
            .follow_links(true)
            .into_iter()
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .filter_map(|entry| {
                let key = entry.path().strip_prefix(dir).ok()?.to_path_buf();
                let masked = entry.metadata().ok()?.len() == 0;
                Some((key, (!masked).then(|| entry.path().to_path_buf())))
            })
            .collect()
    }

    /// Get the root directory for this CFS instance.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Get the primary configuration name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the search prefixes, from the lowest to the highest priority.
    pub fn search_prefixes(&self) -> &[PathBuf] {
        &self.prefixes
    }

    /// Check if the recursive traversal of the drop-in directories is enabled.
    pub fn is_recursive(&self) -> bool {
        self.recursive
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    #[test]
    fn test_uapi_masking_and_ordering() -> std::result::Result<(), Box<dyn std::error::Error>> {
        // Create an isolated temporary directory root
        let tmp_root = tempfile::tempdir()?;
        let root_path = tmp_root.path();

        // 1. Create a mock directory hierarchy
        let usr_share = root_path.join("usr/share");
        let run = root_path.join("run");
        let etc = root_path.join("etc");

        fs::create_dir_all(usr_share.join("myapp.conf.d"))?;
        fs::create_dir_all(run.join("myapp.conf.d"))?;
        fs::create_dir_all(etc.join("myapp.conf.d"))?;

        // 2. Populate Main Files (Etc should override Usr Share)
        File::create(usr_share.join("myapp.conf"))?.write_all(b"usr_content")?;
        File::create(etc.join("myapp.conf"))?.write_all(b"etc_content")?; // Higher priority win

        // 3. Populate Drop-ins & Masking cases
        // 10-link.conf exists in usr, but is masked (0 bytes) in etc
        File::create(usr_share.join("myapp.conf.d/10-link.conf"))?.write_all(b"active")?;
        File::create(etc.join("myapp.conf.d/10-link.conf"))?; // 0 bytes -> MASKED

        // 20-override.conf exists in usr, but is changed in run
        File::create(usr_share.join("myapp.conf.d/20-override.conf"))?.write_all(b"v1")?;
        File::create(run.join("myapp.conf.d/20-override.conf"))?.write_all(b"v2")?;

        // 05-first.conf only exists in etc
        File::create(etc.join("myapp.conf.d/05-first.conf"))?.write_all(b"first")?;

        // 4. Run the code using our mock prefixes
        let uapi = UAPICFS::with_root("myapp.conf", root_path);
        let resolved_files = uapi.files()?;

        // 5. Assertions
        assert_eq!(
            resolved_files.len(),
            3,
            "Should have 1 main file and 2 drop-ins"
        );

        // Verify ordering: Main file (myapp.conf) MUST be first,
        // followed by 05-first.conf, followed by 20-override.conf (lexicographical)
        assert!(resolved_files[0].ends_with("etc/myapp.conf"));
        assert!(resolved_files[1].ends_with("etc/myapp.conf.d/05-first.conf"));
        assert!(resolved_files[2].ends_with("run/myapp.conf.d/20-override.conf"));

        // Verify that 10-link.conf was successfully dropped/masked completely
        for path in &resolved_files {
            assert!(!path.ends_with("10-link.conf"));
        }

        Ok(())
    }

    #[test]
    fn test_uapi_recursive_dropins() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let tmp_root = tempfile::tempdir()?;
        let root_path = tmp_root.path();

        let usr_share = root_path.join("usr/share");
        let run = root_path.join("run");
        let etc = root_path.join("etc");

        fs::create_dir_all(usr_share.join("myapp.conf.d/net"))?;
        fs::create_dir_all(usr_share.join("myapp.conf.d/disk"))?;
        fs::create_dir_all(run.join("myapp.conf.d/net"))?;
        fs::create_dir_all(etc.join("myapp.conf.d/disk"))?;

        // Top level drop-in, unaffected by the recursion
        File::create(usr_share.join("myapp.conf.d/10-top.conf"))?.write_all(b"top")?;

        // net/20-nic.conf is defined in usr, and overridden in run
        File::create(usr_share.join("myapp.conf.d/net/20-nic.conf"))?.write_all(b"v1")?;
        File::create(run.join("myapp.conf.d/net/20-nic.conf"))?.write_all(b"v2")?;

        // disk/30-part.conf is defined in usr, and masked in etc
        File::create(usr_share.join("myapp.conf.d/disk/30-part.conf"))?.write_all(b"active")?;
        File::create(etc.join("myapp.conf.d/disk/30-part.conf"))?; // 0 bytes -> MASKED

        // Same file name in a different subdirectory is a different entry
        File::create(usr_share.join("myapp.conf.d/net/30-part.conf"))?.write_all(b"nic part")?;

        // Without the recursive mode the subdirectories are ignored
        let uapi = UAPICFS::with_root("myapp.conf", root_path);
        assert!(!uapi.is_recursive());
        let resolved_files = uapi.files()?;
        assert_eq!(resolved_files.len(), 1);
        assert!(resolved_files[0].ends_with("usr/share/myapp.conf.d/10-top.conf"));

        // With the recursive mode the same rules apply to the subdirectories
        let uapi = UAPICFS::with_root("myapp.conf", root_path).recursive(true);
        assert!(uapi.is_recursive());
        let resolved_files = uapi.files()?;

        assert_eq!(resolved_files.len(), 3);
        assert!(resolved_files[0].ends_with("usr/share/myapp.conf.d/10-top.conf"));
        assert!(resolved_files[1].ends_with("run/myapp.conf.d/net/20-nic.conf"));
        assert!(resolved_files[2].ends_with("usr/share/myapp.conf.d/net/30-part.conf"));

        // disk/30-part.conf was masked, but net/30-part.conf was not
        for path in &resolved_files {
            assert!(!path.ends_with("disk/30-part.conf"));
        }

        Ok(())
    }

    #[test]
    fn test_uapi_custom_prefixes() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let tmp_root = tempfile::tempdir()?;
        let root_path = tmp_root.path();

        let libexec = root_path.join("usr/libexec");
        let var_lib = root_path.join("var/lib");

        fs::create_dir_all(libexec.join("probes.d"))?;
        fs::create_dir_all(var_lib.join("probes.d"))?;
        fs::create_dir_all(root_path.join("usr/share/probes.d"))?;

        File::create(libexec.join("probes.d/10-bootctl"))?.write_all(b"vendor")?;
        File::create(var_lib.join("probes.d/10-bootctl"))?.write_all(b"local")?;

        // Not searched, as it is not in the configured prefixes
        File::create(root_path.join("usr/share/probes.d/20-snapper"))?.write_all(b"ignored")?;

        let uapi = UAPICFS::with_root("probes", root_path).prefixes(["/usr/libexec", "var/lib"]);

        // The leading separator is ignored
        assert_eq!(
            uapi.search_prefixes(),
            [PathBuf::from("usr/libexec"), PathBuf::from("var/lib")]
        );

        let resolved_files = uapi.files()?;
        assert_eq!(resolved_files.len(), 1);
        assert!(resolved_files[0].ends_with("var/lib/probes.d/10-bootctl"));

        // The default prefixes are still the configuration ones
        let uapi = UAPICFS::with_root("probes", root_path);
        let resolved_files = uapi.files()?;
        assert_eq!(resolved_files.len(), 1);
        assert!(resolved_files[0].ends_with("usr/share/probes.d/20-snapper"));

        Ok(())
    }

    #[test]
    fn test_the_rung_of_a_file_and_the_rest_of_its_path() {
        let root = Path::new("/sysroot");

        // The rung is the index in the ladder, and the rest is what is the same
        // in every prefix of it, which is what names the file that would shadow
        // this one somewhere else
        assert_eq!(
            rung(
                root,
                Path::new("/sysroot/usr/share/detc/templates.d/etc/hostname"),
                SEARCH_PREFIXES
            ),
            Some((0, PathBuf::from("detc/templates.d/etc/hostname")))
        );
        assert_eq!(
            rung(
                root,
                Path::new("/sysroot/etc/detc/templates.d/etc/hostname"),
                SEARCH_PREFIXES
            ),
            Some((2, PathBuf::from("detc/templates.d/etc/hostname")))
        );

        // A ladder of its own, for the programs
        assert_eq!(
            rung(
                root,
                Path::new("/sysroot/var/lib/detc/providers.d/pkg"),
                &["usr/libexec", "run/lib", "var/lib"]
            ),
            Some((2, PathBuf::from("detc/providers.d/pkg")))
        );

        // The highest prefix that holds the path wins, so a ladder whose
        // prefixes nest answers with the more specific one rather than with
        // `lib/detc/x` under `run`
        assert_eq!(
            rung(
                root,
                Path::new("/sysroot/run/lib/detc/x"),
                &["run", "run/lib"]
            ),
            Some((1, PathBuf::from("detc/x")))
        );

        // Nothing is read, so a file that is not there is answered for
        assert_eq!(
            rung(
                root,
                Path::new("/sysroot/etc/detc/nothing"),
                SEARCH_PREFIXES
            ),
            Some((2, PathBuf::from("detc/nothing")))
        );

        // A file outside the root, and one inside it but on no rung
        assert_eq!(
            rung(root, Path::new("/elsewhere/etc/detc/x"), SEARCH_PREFIXES),
            None
        );
        assert_eq!(
            rung(root, Path::new("/sysroot/opt/detc/x"), SEARCH_PREFIXES),
            None
        );
    }
}
