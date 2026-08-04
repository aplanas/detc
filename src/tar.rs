//! A reader and a writer for the subset of `ustar` that a bundle is made of.
//!
//! A bundle is a tar archive because whoever receives one has to be able to
//! look inside it with the tool they already have.  What is written here is the
//! whole of what is accepted: regular files, names that fit in a header, and
//! modes that are permissions.  Symbolic links, hard links, devices, and the
//! pax and GNU extension headers that can rewrite the name of the entry that
//! follows them are refused, because a bundle is unpacked by a process running
//! as root, and an archive that can say more than *this file has these bytes*
//! can say something that was not meant.
//!
//! The blocks themselves are read and written by the `tar` crate.  What this
//! module adds is the refusal, which is the reason it still exists: the crate
//! reads a whole archive gladly, and in particular it *absorbs* the extension
//! headers and applies what they say to the entry behind them.  So the members
//! arrive here raw, one header at a time, and everything that is not a plain
//! file stops the read instead of being skipped over.
//!
//! What is written is reproducible.  Nothing of the machine that ran the writer
//! reaches the archive: no times, no identities, no order but the one the
//! caller gave.  Two builds of the same tree are the same bytes, which is what
//! lets a bundle be checked against the tree it claims to come from.

use std::io::Read;

use crate::Result;

/// The size of a header, of a block of data, and of the unit that everything
/// is padded to.
const BLOCK: usize = 512;

/// The size of the record that a reader of tar archives asks for at once.  The
/// archive is padded to a multiple of it, as every other writer does.
const RECORD: usize = 20 * BLOCK;

/// The largest archive that is read or written.  A bundle carries
/// configuration, so this is far above what one holds, and it bounds what an
/// archive that arrived from somewhere else can make this side allocate.
pub const MAX_SIZE: usize = 32 * 1024 * 1024;

/// The longest name that fits in the `name` field of a header.  A longer one
/// is split over the `prefix` field, which is read here but never written: no
/// path of a bundle comes close, and one name has one spelling.
const MAX_NAME: usize = 100;

/// One member of an archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The path of the member, relative to the top of the archive.
    pub name: String,

    /// The permission bits.  The writer is told what to put there and the
    /// reader reports what it found, so that whoever unpacks the archive
    /// decides what a mode means rather than the archive.
    pub mode: u32,

    /// The contents.
    pub data: Vec<u8>,
}

impl Entry {
    pub fn new(name: impl Into<String>, mode: u32, data: Vec<u8>) -> Self {
        Entry {
            name: name.into(),
            mode,
            data,
        }
    }
}

/// Write the members into an archive, in the order they are given.
pub fn write(entries: &[Entry]) -> Result<Vec<u8>> {
    let mut archive = tar::Builder::new(Vec::new());

    for entry in entries {
        check(&entry.name)?;

        if entry.name.len() > MAX_NAME {
            return err!(
                "A bundle cannot hold {}, whose name is longer than the {MAX_NAME} bytes of a header",
                entry.name
            );
        }

        if entry.mode & !0o777 != 0 {
            return err!(
                "A bundle cannot hold {} with the mode {:o}, which is more than a permission",
                entry.name,
                entry.mode
            );
        }

        // Asked before the member is laid down rather than after, so an archive
        // that would be too large is refused instead of built and then thrown
        // away
        if archive.get_ref().len() + BLOCK + entry.data.len() > MAX_SIZE {
            return too_large();
        }

        archive.append(&header(entry)?, entry.data.as_slice())?;
    }

    // Which ends the archive with the two zero blocks that say so
    let mut archive = archive.into_inner()?;

    // What follows them is the padding of the record that the last one falls
    // in.  The crate stops at the two blocks, and a reader of tar archives asks
    // for a whole record at a time, so the padding is written here as every
    // other writer of one writes it
    archive.resize(archive.len().next_multiple_of(RECORD), 0);

    Ok(archive)
}

/// Read the members of an archive.
///
/// Everything that is not a regular file with a name under the archive is an
/// error and not something to skip: an archive that holds one is not the kind
/// of archive this is, and unpacking the rest of it would be answering a
/// question nobody asked.
pub fn read(archive: &[u8]) -> Result<Vec<Entry>> {
    if archive.len() > MAX_SIZE {
        return too_large();
    }

    // An archive is a sequence of whole blocks, so a length that is not a
    // multiple of one is a file that was cut short of the end of a block.  The
    // one that was cut *inside* a member is caught further down, where the
    // header says how many bytes should have been there
    if !archive.len().is_multiple_of(BLOCK) {
        return err!("The archive ends in the middle of a block");
    }

    let mut entries = Vec::new();
    let mut reader = tar::Archive::new(archive);

    // Raw, so that an extension header arrives as the member it is and is
    // refused below.  Read the other way, it would be swallowed and what it
    // says applied to the member behind it, without a trace in what comes out
    for entry in reader.entries()?.raw(true) {
        let mut entry = entry.map_err(|e| format!("The archive cannot be read: {e}"))?;
        let header = entry.header();

        // `ustar` proper and the GNU flavour of it, which is what the `tar` of
        // a machine writes when it is not told otherwise
        if header.as_ustar().is_none() && header.as_gnu().is_none() {
            return err!("The archive is not in the ustar format");
        }

        let Ok(name) = String::from_utf8(header.path_bytes().into_owned()) else {
            return err!("The archive holds a name that is not valid UTF-8");
        };
        check(&name)?;

        let kind = header.entry_type();
        if !kind.is_file() {
            return err!(
                "The archive holds {name}, {}; a bundle is regular files and nothing else",
                describe(kind)
            );
        }

        let mode = header
            .mode()
            .map_err(|e| format!("The archive does not say the mode of {name}: {e}"))?;
        if mode & !0o777 != 0 {
            return err!(
                "The archive holds {name} with the mode {mode:o}, which is more than a permission"
            );
        }

        let size = header
            .size()
            .map_err(|e| format!("The archive does not say the size of {name}: {e}"))?;

        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;

        // The member is read from what is left of the archive, so a short read
        // is an archive that ends where the header says it should not
        if data.len() as u64 != size {
            return err!("The archive ends in the middle of {name}");
        }

        entries.push(Entry::new(name, mode, data));
    }

    Ok(entries)
}

/// The header of one member, with everything of the machine that wrote it left
/// out: the times are zero, the identities are zero and nameless, and the mode
/// is the one the caller chose.
fn header(entry: &Entry) -> Result<tar::Header> {
    let mut header = tar::Header::new_ustar();

    header
        .set_path(&entry.name)
        .map_err(|e| format!("A bundle cannot hold {}: {e}", entry.name))?;
    header.set_entry_type(tar::EntryType::Regular);
    header.set_mode(entry.mode);
    header.set_size(entry.data.len() as u64);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);

    // Last, because it is the sum of everything above it
    header.set_cksum();

    Ok(header)
}

/// What a member that is not a regular file is, so that the refusal says which
/// of them arrived.
fn describe(kind: tar::EntryType) -> &'static str {
    match kind {
        tar::EntryType::Link => "a hard link",
        tar::EntryType::Symlink => "a symbolic link",
        tar::EntryType::Char => "a character device",
        tar::EntryType::Block => "a block device",
        tar::EntryType::Directory => "a directory",
        tar::EntryType::Fifo => "a named pipe",
        tar::EntryType::Continuous => "a contiguous file",
        tar::EntryType::XHeader | tar::EntryType::XGlobalHeader => {
            "a pax extension header, which can rewrite the entry that follows it"
        }
        tar::EntryType::GNULongName | tar::EntryType::GNULongLink => {
            "a GNU long name header, which can rewrite the entry that follows it"
        }
        _ => "of a kind that this format does not have",
    }
}

/// The names that are accepted, on the way in and on the way out: a relative
/// path of plain components, naming a file.
fn check(name: &str) -> Result<()> {
    if name.is_empty() {
        return err!("A bundle cannot hold an entry with no name");
    }

    if name.contains('\0') {
        return err!("A bundle cannot hold a name with a NUL in it");
    }

    if name.starts_with('/') {
        return err!("A bundle cannot hold {name}, which is an absolute path");
    }

    if name.ends_with('/') {
        return err!("A bundle cannot hold {name}, which names a directory");
    }

    for component in name.split('/') {
        if component == ".." {
            return err!("A bundle cannot hold {name}, which climbs out of the archive");
        }

        if component.is_empty() || component == "." {
            return err!("A bundle cannot hold {name}, whose path is not written plainly");
        }
    }

    Ok(())
}

/// The one refusal that both sides share.
fn too_large<T>() -> Result<T> {
    err!(
        "The archive is larger than the {} MiB that a bundle may hold",
        MAX_SIZE >> 20
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::ops::Range;
    use std::process::Command;

    use log::warn;

    use super::*;

    type TestResult = Result<()>;

    /// Where the fields that a test spoils by hand live in a header.  The
    /// writer no longer knows: it says what it means and the crate lays the
    /// bytes out, which is what leaves these here and nowhere else.
    const NAME: Range<usize> = 0..100;
    const TYPEFLAG: usize = 156;
    const MAGIC: Range<usize> = 257..263;
    const PREFIX: Range<usize> = 345..500;

    /// Where the machine that wrote a header would leave its name and the name
    /// of its group, if it left anything at all.
    const IDENTITY: Range<usize> = 265..329;

    /// A member with text in it.
    fn entry(name: &str, mode: u32, data: &str) -> Entry {
        Entry::new(name, mode, data.as_bytes().to_vec())
    }

    /// An archive of one header, so that a test can spoil a single field of it
    /// and see what reading says.  The checksum is filled in afterwards, so
    /// what is refused is the field and not the arithmetic.
    fn spoiled(patch: impl FnOnce(&mut tar::Header)) -> Vec<u8> {
        let mut header = header(&entry("templates.d/etc/hosts", 0o644, ""))
            .expect("the header of a plain member is written");
        patch(&mut header);
        header.set_cksum();

        let mut archive = header.as_bytes().to_vec();
        archive.resize(archive.len() + 2 * BLOCK, 0);
        archive
    }

    /// Whether GNU tar is here to be read by and written for.
    fn has_tar() -> bool {
        let present = Command::new("tar").arg("--version").output().is_ok();

        if !present {
            warn!("tar is not installed, so the interoperability is not checked here");
        }

        present
    }

    #[test]
    fn a_member_comes_back_as_it_was_written() -> TestResult {
        // Nothing, something that stops just short of a block, exactly a block,
        // and something that spills into the next one
        let entries = vec![
            entry("bundle.yaml", 0o644, ""),
            entry("templates.d/etc/hosts", 0o644, &"x".repeat(BLOCK - 1)),
            entry("providers.d/unit", 0o755, &"y".repeat(BLOCK)),
            entry("probes/system.d/10-net", 0o755, &"z".repeat(BLOCK + 1)),
        ];

        assert_eq!(read(&write(&entries)?)?, entries);

        Ok(())
    }

    #[test]
    fn an_archive_ends_with_two_zero_blocks_in_a_whole_record() -> TestResult {
        let archive = write(&[entry("bundle.yaml", 0o644, "name: fleet\n")])?;

        assert_eq!(archive.len() % RECORD, 0);
        assert!(archive[2 * BLOCK..].iter().all(|byte| *byte == 0));

        // An archive of nothing is still an archive
        assert!(read(&write(&[])?)?.is_empty());

        Ok(())
    }

    #[test]
    fn only_regular_files_are_read() -> TestResult {
        // A file is either kind, as the oldest writers left the field unset
        for kind in [b'0', 0] {
            let archive = spoiled(|header| header.as_mut_bytes()[TYPEFLAG] = kind);
            assert_eq!(read(&archive)?.len(), 1);
        }

        for (kind, said) in [
            (b'1', "a hard link"),
            (b'2', "a symbolic link"),
            (b'3', "a character device"),
            (b'4', "a block device"),
            (b'5', "a directory"),
            (b'6', "a named pipe"),
            (b'x', "a pax extension header"),
            (b'L', "a GNU long name header"),
        ] {
            let archive = spoiled(|header| header.as_mut_bytes()[TYPEFLAG] = kind);
            let error = read(&archive)
                .expect_err("what is not a regular file is refused")
                .to_string();

            assert!(error.contains(said), "{error}");
        }

        Ok(())
    }

    #[test]
    fn a_name_that_leaves_the_archive_is_refused() -> TestResult {
        for (name, said) in [
            ("/etc/passwd", "an absolute path"),
            ("../../etc/passwd", "climbs out"),
            ("templates.d/../../etc/passwd", "climbs out"),
            ("./templates.d/etc/hosts", "not written plainly"),
            ("templates.d/", "names a directory"),
        ] {
            // Neither on the way in
            let archive = spoiled(|header| {
                let block = header.as_mut_bytes();
                block[NAME].fill(0);
                block[..name.len()].copy_from_slice(name.as_bytes());
            });
            let error = read(&archive)
                .expect_err("a name that leaves the archive is refused")
                .to_string();
            assert!(error.contains(said), "{name}: {error}");

            // nor on the way out
            let error = write(&[entry(name, 0o644, "")])
                .expect_err("a name that leaves the archive is not written")
                .to_string();
            assert!(error.contains(said), "{name}: {error}");
        }

        Ok(())
    }

    #[test]
    fn a_mode_that_is_more_than_a_permission_is_refused() -> TestResult {
        for mode in [0o4755, 0o2755, 0o1755] {
            let archive = spoiled(|header| header.set_mode(mode));
            let error = read(&archive)
                .expect_err("a mode that is more than a permission is refused")
                .to_string();
            assert!(error.contains("more than a permission"), "{error}");

            let error = write(&[entry("providers.d/unit", mode, "")])
                .expect_err("a mode that is more than a permission is not written")
                .to_string();
            assert!(error.contains("more than a permission"), "{error}");
        }

        Ok(())
    }

    #[test]
    fn a_header_that_does_not_add_up_is_refused() -> TestResult {
        // The name is changed after the checksum was written, which is what a
        // header rewritten in place looks like
        let mut archive = spoiled(|_| {});
        archive[..11].copy_from_slice(b"passwd\0\0\0\0\0");

        let error = read(&archive)
            .expect_err("a header that was tampered with is refused")
            .to_string();
        assert!(error.contains("checksum"), "{error}");

        Ok(())
    }

    #[test]
    fn a_header_of_something_else_is_refused() -> TestResult {
        let archive = spoiled(|header| header.as_mut_bytes()[MAGIC].copy_from_slice(b"nope!\0"));

        let error = read(&archive)
            .expect_err("what is not a tar archive is refused")
            .to_string();
        assert!(error.contains("ustar"), "{error}");

        Ok(())
    }

    #[test]
    fn a_name_that_does_not_fit_a_header_is_refused() -> TestResult {
        let name = format!("templates.d/{}", "a".repeat(MAX_NAME));

        let error = write(&[entry(&name, 0o644, "")])
            .expect_err("a name that does not fit is refused")
            .to_string();
        assert!(error.contains("longer than"), "{error}");

        Ok(())
    }

    #[test]
    fn an_archive_that_was_cut_is_refused() -> TestResult {
        let archive = write(&[entry("bundle.yaml", 0o644, &"x".repeat(BLOCK + 1))])?;

        // In the middle of the contents of a member
        let error = read(&archive[..2 * BLOCK])
            .expect_err("an archive that was cut is refused")
            .to_string();
        assert!(
            error.contains("ends in the middle of bundle.yaml"),
            "{error}"
        );

        // and in the middle of a block, which is where a header lives
        let error = read(&archive[..BLOCK / 2])
            .expect_err("an archive that was cut is refused")
            .to_string();
        assert!(error.contains("ends in the middle of a block"), "{error}");

        Ok(())
    }

    #[test]
    fn a_name_that_needed_the_prefix_field_is_read_whole() -> TestResult {
        let archive = spoiled(|header| {
            let block = header.as_mut_bytes();
            block[NAME].fill(0);
            block[..9].copy_from_slice(b"hosts.tpl");
            block[PREFIX][..11].copy_from_slice(b"templates.d");
        });

        assert_eq!(read(&archive)?[0].name, "templates.d/hosts.tpl");

        Ok(())
    }

    #[test]
    fn nothing_of_the_machine_that_wrote_it_reaches_the_archive() -> TestResult {
        let one = write(&[entry("bundle.yaml", 0o644, "name: fleet\n")])?;
        std::thread::sleep(std::time::Duration::from_millis(10));
        let other = write(&[entry("bundle.yaml", 0o644, "name: fleet\n")])?;

        assert_eq!(one, other);

        // No identity and no time is in there to make them differ later
        let mut header = tar::Header::new_old();
        header.as_mut_bytes().copy_from_slice(&one[..BLOCK]);

        assert_eq!(header.uid()?, 0);
        assert_eq!(header.gid()?, 0);
        assert_eq!(header.mtime()?, 0);
        assert!(one[IDENTITY].iter().all(|byte| *byte == 0));

        Ok(())
    }

    #[test]
    fn gnu_tar_reads_what_is_written() -> TestResult {
        if !has_tar() {
            return Ok(());
        }

        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();
        let path = root.join("payload.tar");

        fs::write(
            &path,
            write(&[
                entry("bundle.yaml", 0o644, "name: fleet\nversion: 1\n"),
                entry("providers.d/unit", 0o755, "#!/bin/sh\n"),
            ])?,
        )?;

        let listing = Command::new("tar").arg("tvf").arg(&path).output()?;
        assert!(listing.status.success(), "{listing:?}");

        let listing = String::from_utf8(listing.stdout)?;
        assert!(listing.contains("-rw-r--r--"), "{listing}");
        assert!(listing.contains("bundle.yaml"), "{listing}");
        assert!(listing.contains("-rwxr-xr-x"), "{listing}");
        assert!(listing.contains("providers.d/unit"), "{listing}");

        // And what it unpacks is what went in
        let unpacked = root.join("out");
        fs::create_dir(&unpacked)?;
        let status = Command::new("tar")
            .arg("xf")
            .arg(&path)
            .arg("-C")
            .arg(&unpacked)
            .status()?;
        assert!(status.success());

        assert_eq!(
            fs::read_to_string(unpacked.join("bundle.yaml"))?,
            "name: fleet\nversion: 1\n"
        );

        Ok(())
    }

    #[test]
    fn what_gnu_tar_writes_is_read() -> TestResult {
        if !has_tar() {
            return Ok(());
        }

        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        fs::create_dir_all(root.join("templates.d"))?;
        fs::write(root.join("bundle.yaml"), "name: fleet\n")?;
        fs::write(root.join("templates.d/hosts"), "127.0.0.1 localhost\n")?;

        let path = root.join("payload.tar");
        let status = Command::new("tar")
            .arg("--format=ustar")
            .arg("-cf")
            .arg(&path)
            .arg("-C")
            .arg(root)
            .arg("bundle.yaml")
            .arg("templates.d/hosts")
            .status()?;
        assert!(status.success());

        let entries = read(&fs::read(&path)?)?;

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "bundle.yaml");
        assert_eq!(entries[0].data, b"name: fleet\n");
        assert_eq!(entries[1].name, "templates.d/hosts");
        assert_eq!(entries[1].data, b"127.0.0.1 localhost\n");

        Ok(())
    }

    #[test]
    fn what_gnu_tar_writes_of_a_tree_is_refused() -> TestResult {
        if !has_tar() {
            return Ok(());
        }

        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        fs::create_dir_all(root.join("templates.d"))?;
        fs::write(root.join("templates.d/hosts"), "127.0.0.1 localhost\n")?;
        std::os::unix::fs::symlink("hosts", root.join("templates.d/hostname"))?;

        let path = root.join("payload.tar");
        let status = Command::new("tar")
            .arg("--format=ustar")
            .arg("-cf")
            .arg(&path)
            .arg("-C")
            .arg(root)
            .arg("templates.d")
            .status()?;
        assert!(status.success());

        // The directory entry comes first, and it is already too much
        let error = read(&fs::read(&path)?)
            .expect_err("an archive of a tree holds more than files")
            .to_string();
        assert!(error.contains("a directory"), "{error}");

        Ok(())
    }
}
