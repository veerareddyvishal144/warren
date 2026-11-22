//! Building and editing guest root filesystems without ever mounting them:
//! `mkfs.ext4 -d` packs an exported directory, `debugfs` reads and writes files
//! inside an image, `e2fsck` + `resize2fs` grow it, and sparse-aware copies and
//! tar.gz archives move home directories around.

use std::{
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use warren_core::{Error, Result};

use crate::host::{self, lossy};

/// Bytes in one "GB" of the shape; disks are sized in GiB like the containers.
pub const GIB: u64 = 1 << 30;
/// Where the gateway init reads the instance environment.
pub const ENV_FILE: &str = "etc/warren.env";
/// Resolvers written into the guest; the kernel `ip=` line sets no DNS.
pub const RESOLVERS: [&str; 2] = ["1.1.1.1", "8.8.8.8"];

/// `KEY=VALUE` lines, sorted by key, newline-terminated.
pub fn render_env(env: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for (k, v) in env {
        out.push_str(k);
        out.push('=');
        out.push_str(v);
        out.push('\n');
    }
    out
}

/// Inverse of [`render_env`]; blank lines and `#` comments are skipped.
pub fn parse_env(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .filter_map(|l| {
            l.split_once('=')
                .map(|(k, v)| (k.trim().to_string(), v.to_string()))
        })
        .collect()
}

/// Map an absolute guest path to its location under an exported root directory.
pub fn guest_to_host(rootfs_dir: &Path, guest_path: &str) -> PathBuf {
    rootfs_dir.join(guest_path.trim_start_matches('/'))
}

/// The last component of a guest path, as `debugfs rdump` names its output.
pub fn guest_basename(guest_path: &str) -> Result<String> {
    let name = guest_path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string();
    if name.is_empty() || name == "." || name == ".." {
        return Err(Error::invalid(format!(
            "mount path {guest_path:?} must name a directory below /"
        )));
    }
    Ok(name)
}

/// Write a file with `mode` into an exported tree, creating its parents.
pub(crate) fn write_file(path: &Path, contents: &str, mode: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::internal(format!("mkdir {}: {e}", parent.display())))?;
    }
    // Images ship resolv.conf and friends as symlinks into systemd; replace them.
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && (meta.file_type().is_symlink() || !meta.is_file())
    {
        std::fs::remove_file(path)
            .map_err(|e| Error::internal(format!("remove {}: {e}", path.display())))?;
    }
    std::fs::write(path, contents)
        .map_err(|e| Error::internal(format!("write {}: {e}", path.display())))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| Error::internal(format!("chmod {}: {e}", path.display())))
}

/// Write `/etc/warren.env` (mode 0600) into an exported root directory.
pub fn write_env_file(rootfs_dir: &Path, env: &BTreeMap<String, String>) -> Result<()> {
    write_file(&rootfs_dir.join(ENV_FILE), &render_env(env), 0o600)
}

/// `/etc/hostname` for an instance.
pub fn render_hostname(id: &str) -> String {
    format!("{id}\n")
}

/// `/etc/hosts` for an instance.
pub fn render_hosts(id: &str) -> String {
    format!("127.0.0.1\tlocalhost\n127.0.1.1\t{id}\n::1\tlocalhost ip6-localhost ip6-loopback\n")
}

/// `/etc/resolv.conf` for every guest.
pub fn render_resolv() -> String {
    RESOLVERS
        .iter()
        .map(|r| format!("nameserver {r}\n"))
        .collect()
}

/// Hostname, hosts and resolvers for the guest, and the mount path directory
/// in case the image lacks it.
pub fn write_identity_files(rootfs_dir: &Path, id: &str, mount_path: &str) -> Result<()> {
    write_file(
        &rootfs_dir.join("etc/hostname"),
        &render_hostname(id),
        0o644,
    )?;
    write_file(&rootfs_dir.join("etc/hosts"), &render_hosts(id), 0o644)?;
    write_file(&rootfs_dir.join("etc/resolv.conf"), &render_resolv(), 0o644)?;
    let home = guest_to_host(rootfs_dir, mount_path);
    std::fs::create_dir_all(&home)
        .map_err(|e| Error::internal(format!("mkdir {}: {e}", home.display())))?;
    Ok(())
}

/// Create a sparse `disk_gb` GiB file at `image` and pack `source_dir` into it.
/// Inode tables are left for the kernel to initialise lazily; the journal is
/// written out here, because the first read-write open of an image with an
/// uninitialised journal (the `debugfs` write that gives a cached copy its
/// identity) would otherwise spend seconds on it.
pub async fn mkfs(source_dir: &Path, image: &Path, disk_gb: u32) -> Result<()> {
    if disk_gb == 0 {
        return Err(Error::invalid("disk size must be at least 1 GB"));
    }
    let mkfs = host::tool("mkfs.ext4")?;
    let f = std::fs::File::create(image)
        .map_err(|e| Error::internal(format!("create {}: {e}", image.display())))?;
    f.set_len(u64::from(disk_gb) * GIB)
        .map_err(|e| Error::internal(format!("size {}: {e}", image.display())))?;
    drop(f);
    let src = source_dir.to_string_lossy().into_owned();
    let img = image.to_string_lossy().into_owned();
    host::run(
        &mkfs,
        &[
            "-F",
            "-q",
            "-E",
            "lazy_itable_init=1,lazy_journal_init=0",
            "-d",
            &src,
            &img,
        ],
    )
    .await?;
    Ok(())
}

/// Current size of an image in whole GiB, rounded up.
pub fn size_gb(image: &Path) -> Result<u32> {
    let len = std::fs::metadata(image)
        .map_err(|e| Error::internal(format!("stat {}: {e}", image.display())))?
        .len();
    Ok(u32::try_from(len.div_ceil(GIB)).unwrap_or(u32::MAX))
}

/// Grow an image to `disk_gb` GiB: extend the file, check it (the VM mounted
/// it, so resize2fs insists on a check) and resize the filesystem. Never
/// shrinks; returns false when the image is already that large.
pub async fn grow(image: &Path, disk_gb: u32) -> Result<bool> {
    let target = u64::from(disk_gb) * GIB;
    let len = std::fs::metadata(image)
        .map_err(|e| Error::internal(format!("stat {}: {e}", image.display())))?
        .len();
    if target <= len {
        return Ok(false);
    }
    let e2fsck = host::tool("e2fsck")?;
    let resize2fs = host::tool("resize2fs")?;
    let img = image.to_string_lossy().into_owned();
    let out = tokio::process::Command::new(&e2fsck)
        .args(["-f", "-y", &img])
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| Error::internal(format!("e2fsck: {e}")))?;
    // 0 clean, 1 errors corrected, 2 corrected and a reboot would be needed.
    if !matches!(out.status.code(), Some(0..=2)) {
        return Err(Error::internal(format!(
            "e2fsck {img} failed ({}): {}",
            out.status,
            lossy(&out.stdout) + &lossy(&out.stderr)
        )));
    }
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(image)
        .map_err(|e| Error::internal(format!("open {img}: {e}")))?;
    f.set_len(target)
        .map_err(|e| Error::internal(format!("extend {img}: {e}")))?;
    drop(f);
    host::run(&resize2fs, &[&img]).await?;
    Ok(true)
}

/// Bytes actually allocated to a (sparse) file: 512-byte blocks from stat.
pub fn allocated_bytes(path: &Path) -> Result<i64> {
    match std::fs::metadata(path) {
        Ok(m) => Ok(i64::try_from(m.blocks())
            .unwrap_or(i64::MAX)
            .saturating_mul(512)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(Error::internal(format!("stat {}: {e}", path.display()))),
    }
}

/// Run debugfs commands against an image. debugfs exits 0 even when a command
/// fails, so stderr is returned for the caller to interpret.
async fn debugfs(image: &Path, writable: bool, request: &str) -> Result<(String, String)> {
    let debugfs = host::tool("debugfs")?;
    let img = image.to_string_lossy().into_owned();
    let mut args = vec!["-R", request];
    if writable {
        args.insert(0, "-w");
    }
    args.push(&img);
    let out = host::run(&debugfs, &args).await?;
    Ok((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        lossy(&out.stderr),
    ))
}

/// Lines of debugfs stderr that are neither the version banner nor the
/// ownership complaints an unprivileged run always produces.
fn debugfs_errors(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .map(str::trim)
        .filter(|l| {
            !l.is_empty() && !l.starts_with("debugfs ") && !l.contains("while changing ownership")
        })
        .collect()
}

fn is_not_found(errors: &[&str]) -> bool {
    errors.iter().any(|l| l.contains("File not found"))
}

/// `debugfs -R "rdump <guest_path> <dest_dir>"`: copy a guest directory out of
/// the image. Returns the dumped directory (`dest_dir/<basename>`), or `None`
/// when the guest path does not exist.
pub async fn rdump(image: &Path, guest_path: &str, dest_dir: &Path) -> Result<Option<PathBuf>> {
    let name = guest_basename(guest_path)?;
    std::fs::create_dir_all(dest_dir)
        .map_err(|e| Error::internal(format!("mkdir {}: {e}", dest_dir.display())))?;
    let request = format!(
        "rdump {} {}",
        guest_path.trim_end_matches('/'),
        dest_dir.display()
    );
    let (_, stderr) = debugfs(image, false, &request).await?;
    let errors = debugfs_errors(&stderr);
    if is_not_found(&errors) {
        return Ok(None);
    }
    let dumped = dest_dir.join(name);
    if !errors.is_empty() {
        tracing::warn!(image = %image.display(), path = guest_path, errors = ?errors, "debugfs rdump reported problems");
    }
    if !dumped.exists() {
        return Err(Error::internal(format!(
            "debugfs rdump {guest_path} produced nothing: {}",
            errors.join("; ")
        )));
    }
    Ok(Some(dumped))
}

/// Contents of a regular file inside the image, or `None` when it is missing.
pub async fn cat(image: &Path, guest_path: &str) -> Result<Option<String>> {
    let (stdout, stderr) = debugfs(image, false, &format!("cat {guest_path}")).await?;
    let errors = debugfs_errors(&stderr);
    if is_not_found(&errors) {
        return Ok(None);
    }
    if !errors.is_empty() {
        return Err(Error::internal(format!(
            "debugfs cat {guest_path}: {}",
            errors.join("; ")
        )));
    }
    Ok(Some(stdout))
}

/// True when the guest path exists in the image.
pub async fn exists(image: &Path, guest_path: &str) -> Result<bool> {
    let (_, stderr) = debugfs(image, false, &format!("stat {guest_path}")).await?;
    Ok(!is_not_found(&debugfs_errors(&stderr)))
}

/// Permission bits of a guest path (`Mode:  0755` in `debugfs stat`), or
/// `None` when it does not exist.
#[cfg(test)]
pub async fn stat_mode(image: &Path, guest_path: &str) -> Result<Option<u32>> {
    let (stdout, stderr) = debugfs(image, false, &format!("stat {guest_path}")).await?;
    if is_not_found(&debugfs_errors(&stderr)) {
        return Ok(None);
    }
    Ok(stdout
        .split("Mode:")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|m| u32::from_str_radix(m, 8).ok()))
}

/// Replace (or create) a regular file inside the image with `contents` and
/// `mode`, then read it back to be sure. The parent directory must exist.
pub async fn put_file(image: &Path, guest_path: &str, contents: &str, mode: u32) -> Result<()> {
    put_files(image, &[(guest_path, contents, mode)]).await
}

/// [`put_file`] for several `(guest path, contents, mode)` at once: one
/// debugfs run writes them all, then each is read back. Parent directories
/// must exist.
pub async fn put_files(image: &Path, files: &[(&str, &str, u32)]) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let debugfs_bin = host::tool("debugfs")?;
    let mut scratch_files = Vec::with_capacity(files.len());
    let mut script = String::new();
    for (i, (guest_path, contents, mode)) in files.iter().enumerate() {
        let scratch = image.with_extension(format!("put{i}"));
        std::fs::write(&scratch, contents)
            .map_err(|e| Error::internal(format!("write {}: {e}", scratch.display())))?;
        script.push_str(&format!(
            "rm {guest_path}\nwrite {} {guest_path}\nset_inode_field {guest_path} mode 0{:o}\n",
            scratch.display(),
            0o100000 | (mode & 0o7777)
        ));
        scratch_files.push(scratch);
    }
    let cmds = image.with_extension("cmds");
    std::fs::write(&cmds, script)
        .map_err(|e| Error::internal(format!("write {}: {e}", cmds.display())))?;
    let img = image.to_string_lossy().into_owned();
    let cmdfile = cmds.to_string_lossy().into_owned();
    let result = host::run(&debugfs_bin, &["-w", "-f", &cmdfile, &img]).await;
    for scratch in &scratch_files {
        let _ = std::fs::remove_file(scratch);
    }
    let _ = std::fs::remove_file(&cmds);
    result?;
    for (guest_path, contents, _) in files {
        match cat(image, guest_path).await? {
            Some(back) if back == *contents => {}
            _ => {
                return Err(Error::internal(format!(
                    "debugfs could not write {guest_path} into {}",
                    image.display()
                )));
            }
        }
    }
    Ok(())
}

/// Block size of an ext4 image, from `debugfs show_super_stats`.
pub async fn block_size(image: &Path) -> Result<u32> {
    let (stdout, stderr) = debugfs(image, false, "show_super_stats -h").await?;
    let errors = debugfs_errors(&stderr);
    if !errors.is_empty() {
        return Err(Error::internal(format!(
            "debugfs show_super_stats: {}",
            errors.join("; ")
        )));
    }
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("Block size:"))
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|bs| *bs > 0)
        .ok_or_else(|| Error::internal("debugfs show_super_stats: no block size"))
}

/// The blocks holding a file's data, in file order (`debugfs blocks`), or
/// `None` when the file does not exist. An empty file has no blocks.
pub async fn file_blocks(image: &Path, guest_path: &str) -> Result<Option<Vec<u64>>> {
    let (stdout, stderr) = debugfs(image, false, &format!("blocks {guest_path}")).await?;
    let errors = debugfs_errors(&stderr);
    if is_not_found(&errors) {
        return Ok(None);
    }
    if !errors.is_empty() {
        return Err(Error::internal(format!(
            "debugfs blocks {guest_path}: {}",
            errors.join("; ")
        )));
    }
    let blocks = stdout
        .split_whitespace()
        .map(|b| {
            b.parse::<u64>()
                .map_err(|e| Error::internal(format!("debugfs blocks {guest_path}: {b:?}: {e}")))
        })
        .collect::<Result<Vec<u64>>>()?;
    Ok(Some(blocks))
}

/// Overwrite a file's data in place, block by block, without opening the
/// filesystem: `contents` goes into `blocks` (of `block_size` bytes each) at
/// their offsets in the image. The file's size in its inode is untouched, so
/// `contents` must be exactly as long as the file; the caller pads it.
pub fn patch_blocks(image: &Path, blocks: &[u64], block_size: u32, contents: &[u8]) -> Result<()> {
    use std::os::unix::fs::FileExt;
    let bs = block_size as usize;
    if contents.len() > blocks.len() * bs {
        return Err(Error::internal(format!(
            "{} bytes do not fit in {} blocks of {bs}",
            contents.len(),
            blocks.len()
        )));
    }
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(image)
        .map_err(|e| Error::internal(format!("open {}: {e}", image.display())))?;
    for (chunk, block) in contents.chunks(bs).zip(blocks) {
        f.write_all_at(chunk, block * block_size as u64)
            .map_err(|e| {
                Error::internal(format!("write block {block} of {}: {e}", image.display()))
            })?;
    }
    Ok(())
}

/// Read a file's data back from its blocks (the inverse of [`patch_blocks`]):
/// `size` bytes from `blocks` of `block_size` each.
pub fn read_blocks(image: &Path, blocks: &[u64], block_size: u32, size: usize) -> Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    let bs = block_size as usize;
    if size > blocks.len() * bs {
        return Err(Error::internal(format!(
            "{size} bytes do not fit in {} blocks of {bs}",
            blocks.len()
        )));
    }
    let f = std::fs::File::open(image)
        .map_err(|e| Error::internal(format!("open {}: {e}", image.display())))?;
    let mut out = vec![0u8; size];
    for (chunk, block) in out.chunks_mut(bs).zip(blocks) {
        f.read_exact_at(chunk, block * block_size as u64)
            .map_err(|e| {
                Error::internal(format!("read block {block} of {}: {e}", image.display()))
            })?;
    }
    Ok(out)
}

/// `text` followed by newlines up to exactly `size` bytes, or `None` when
/// it is longer. Blank lines are nothing to the readers of these files.
pub fn pad_with_newlines(text: &str, size: usize) -> Option<String> {
    if text.len() > size {
        return None;
    }
    let mut out = String::with_capacity(size);
    out.push_str(text);
    while out.len() < size {
        out.push('\n');
    }
    Some(out)
}

/// Copy a disk image keeping holes. On Linux the source's data extents are
/// found with `SEEK_DATA`/`SEEK_HOLE` and only those are read, so a mostly
/// empty image costs its allocated size, not its apparent size; elsewhere,
/// or when the filesystem cannot report holes, every chunk is read and the
/// all-zero ones become holes in the copy.
pub fn sparse_copy(src: &Path, dst: &Path) -> Result<()> {
    let mut from = std::fs::File::open(src)
        .map_err(|e| Error::internal(format!("open {}: {e}", src.display())))?;
    let len = from.metadata().map_err(Error::internal)?.len();
    let mut to = std::fs::File::create(dst)
        .map_err(|e| Error::internal(format!("create {}: {e}", dst.display())))?;
    #[cfg(target_os = "linux")]
    if copy_data_extents(&mut from, &mut to, len)? {
        return Ok(());
    }
    let mut buf = vec![0u8; 1 << 20];
    let mut offset: u64 = 0;
    loop {
        let n = from.read(&mut buf).map_err(Error::internal)?;
        if n == 0 {
            break;
        }
        if buf[..n].iter().all(|b| *b == 0) {
            to.seek(SeekFrom::Current(n as i64))
                .map_err(Error::internal)?;
        } else {
            to.write_all(&buf[..n]).map_err(Error::internal)?;
        }
        offset += n as u64;
    }
    to.set_len(len.max(offset)).map_err(Error::internal)?;
    to.flush().map_err(Error::internal)?;
    Ok(())
}

/// Copy only the data extents of `from` into `to` (same offsets), leaving the
/// holes. `Ok(false)` when the filesystem does not support `SEEK_DATA`, so
/// the caller falls back to scanning; the destination is untouched then.
#[cfg(target_os = "linux")]
fn copy_data_extents(from: &mut std::fs::File, to: &mut std::fs::File, len: u64) -> Result<bool> {
    use nix::{
        errno::Errno,
        unistd::{Whence, lseek},
    };
    let mut buf = vec![0u8; 1 << 20];
    let mut pos: i64 = 0;
    let end = i64::try_from(len).map_err(|_| Error::internal("image too large"))?;
    while pos < end {
        let data = match lseek(&*from, pos, Whence::SeekData) {
            Ok(off) => off,
            Err(Errno::ENXIO) => break,
            Err(Errno::EINVAL | Errno::ENOTSUP | Errno::EOPNOTSUPP) if pos == 0 => {
                return Ok(false);
            }
            Err(e) => return Err(Error::internal(format!("seek data: {e}"))),
        };
        let hole = match lseek(&*from, data, Whence::SeekHole) {
            Ok(off) => off.min(end),
            Err(Errno::ENXIO) => end,
            Err(e) => return Err(Error::internal(format!("seek hole: {e}"))),
        };
        from.seek(SeekFrom::Start(data as u64))
            .map_err(Error::internal)?;
        to.seek(SeekFrom::Start(data as u64))
            .map_err(Error::internal)?;
        let mut remaining = (hole - data) as u64;
        while remaining > 0 {
            let want = buf
                .len()
                .min(usize::try_from(remaining).unwrap_or(usize::MAX));
            let n = from.read(&mut buf[..want]).map_err(Error::internal)?;
            if n == 0 {
                break;
            }
            to.write_all(&buf[..n]).map_err(Error::internal)?;
            remaining -= n as u64;
        }
        pos = hole;
    }
    to.set_len(len).map_err(Error::internal)?;
    to.flush().map_err(Error::internal)?;
    Ok(true)
}

/// Pack the contents of `dir` (entries relative to it) into a gzip tarball and
/// return the archive size.
pub fn pack_dir_gz(dir: &Path, dest: &Path) -> Result<i64> {
    let file = std::fs::File::create(dest)
        .map_err(|e| Error::internal(format!("create {}: {e}", dest.display())))?;
    let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);
    tar.follow_symlinks(false);
    tar.append_dir_all(".", dir)
        .map_err(|e| Error::internal(format!("pack {}: {e}", dir.display())))?;
    let gz = tar
        .into_inner()
        .map_err(|e| Error::internal(format!("finish tar: {e}")))?;
    let file = gz
        .finish()
        .map_err(|e| Error::internal(format!("finish gzip: {e}")))?;
    let size = file.metadata().map_err(Error::internal)?.len();
    Ok(i64::try_from(size).unwrap_or(i64::MAX))
}

/// Extract a gzip tarball produced by [`pack_dir_gz`] into `dest`.
pub fn unpack_gz(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)
        .map_err(|e| Error::internal(format!("mkdir {}: {e}", dest.display())))?;
    let file = std::fs::File::open(src)
        .map_err(|e| Error::internal(format!("open {}: {e}", src.display())))?;
    let mut archive =
        tar::Archive::new(flate2::read::GzDecoder::new(std::io::BufReader::new(file)));
    crate::image::configure_unpack(&mut archive);
    archive
        .unpack(dest)
        .map_err(|e| Error::internal(format!("unpack {}: {e}", src.display())))
}

/// Remove a directory tree if it exists.
pub fn remove_tree(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::internal(format!("remove {}: {e}", path.display()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TempDir, have_e2fsprogs};

    #[test]
    fn env_round_trip() {
        let env = BTreeMap::from([
            ("B".to_string(), "two=2".to_string()),
            ("A".to_string(), "one".to_string()),
        ]);
        let text = render_env(&env);
        assert_eq!(text, "A=one\nB=two=2\n");
        assert_eq!(parse_env(&text), env);
        assert_eq!(
            parse_env("# c\n\nX=1\nbad\n").get("X").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn guest_paths() {
        assert_eq!(
            guest_to_host(Path::new("/r"), "/home/agent"),
            PathBuf::from("/r/home/agent")
        );
        assert_eq!(guest_basename("/home/agent/").unwrap(), "agent");
        assert!(guest_basename("/").is_err());
        assert!(guest_basename("").is_err());
    }

    #[test]
    fn identity_files_replace_symlinks() {
        let tmp = TempDir::new("fc-identity");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::os::unix::fs::symlink(
            "/run/systemd/resolve/stub-resolv.conf",
            root.join("etc/resolv.conf"),
        )
        .unwrap();
        write_identity_files(root, "ab12cd34ef", "/home/agent").unwrap();
        let resolv = std::fs::read_to_string(root.join("etc/resolv.conf")).unwrap();
        assert!(resolv.contains("nameserver 1.1.1.1"));
        assert_eq!(
            std::fs::read_to_string(root.join("etc/hostname")).unwrap(),
            "ab12cd34ef\n"
        );
        assert!(root.join("home/agent").is_dir());
        let env = BTreeMap::from([("K".to_string(), "v".to_string())]);
        write_env_file(root, &env).unwrap();
        let meta = std::fs::metadata(root.join(ENV_FILE)).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::read_to_string(root.join(ENV_FILE)).unwrap(),
            "K=v\n"
        );
    }

    #[test]
    fn pads_and_patches_blocks() {
        assert_eq!(pad_with_newlines("ab\n", 5).as_deref(), Some("ab\n\n\n"));
        assert_eq!(pad_with_newlines("abcdef", 5), None);
        assert_eq!(pad_with_newlines("", 2).as_deref(), Some("\n\n"));
        let tmp = TempDir::new("fc-patch");
        let img = tmp.path().join("img");
        std::fs::write(&img, vec![0xffu8; 4 * 512]).unwrap();
        // Blocks 3 and 1 (in file order), 512 bytes each.
        let data: Vec<u8> = (0..700u32).map(|i| (i % 251) as u8).collect();
        patch_blocks(&img, &[3, 1], 512, &data).unwrap();
        let back = std::fs::read(&img).unwrap();
        assert_eq!(&back[3 * 512..4 * 512], &data[..512]);
        assert_eq!(&back[512..512 + 188], &data[512..]);
        assert_eq!(&back[512 + 188..1024], &vec![0xffu8; 512 - 188][..]);
        assert_eq!(&back[..512], &vec![0xffu8; 512][..]);
        assert!(patch_blocks(&img, &[0], 512, &data).is_err(), "too long");
        assert_eq!(read_blocks(&img, &[3, 1], 512, 700).unwrap(), data);
        assert!(read_blocks(&img, &[3], 512, 700).is_err());
    }

    #[test]
    #[cfg(target_os = "linux")] // APFS reports allocated blocks differently; the copy only runs on Linux hosts
    fn sparse_copy_keeps_holes() {
        let tmp = TempDir::new("fc-sparse");
        let src = tmp.path().join("src.img");
        let dst = tmp.path().join("dst.img");
        let mut f = std::fs::File::create(&src).unwrap();
        f.set_len(8 << 20).unwrap();
        f.seek(SeekFrom::Start(3 << 20)).unwrap();
        f.write_all(b"payload").unwrap();
        drop(f);
        sparse_copy(&src, &dst).unwrap();
        assert_eq!(std::fs::metadata(&dst).unwrap().len(), 8 << 20);
        let mut back = vec![0u8; 7];
        let mut f = std::fs::File::open(&dst).unwrap();
        f.seek(SeekFrom::Start(3 << 20)).unwrap();
        f.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"payload");
        assert!(
            allocated_bytes(&dst).unwrap() < (4 << 20),
            "copy should stay sparse"
        );
        assert_eq!(allocated_bytes(&tmp.path().join("missing")).unwrap(), 0);

        // Several extents, data at both ends: the copy is byte-identical and
        // still sparse.
        let src = tmp.path().join("src2.img");
        let dst = tmp.path().join("dst2.img");
        let mut f = std::fs::File::create(&src).unwrap();
        f.write_all(&vec![0x5au8; (1 << 20) + 5]).unwrap();
        f.seek(SeekFrom::Start(5 << 20)).unwrap();
        f.write_all(b"middle").unwrap();
        f.seek(SeekFrom::Start((16 << 20) - 3)).unwrap();
        f.write_all(b"end").unwrap();
        drop(f);
        sparse_copy(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&src).unwrap(), std::fs::read(&dst).unwrap());
        assert_eq!(std::fs::metadata(&dst).unwrap().len(), 16 << 20);
        assert!(
            allocated_bytes(&dst).unwrap() < (3 << 20),
            "copy should stay sparse"
        );
    }

    #[test]
    fn tar_gz_round_trip() {
        let tmp = TempDir::new("fc-targz");
        let src = tmp.path().join("home");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("sub/f.txt"), "hello").unwrap();
        std::os::unix::fs::symlink("sub/f.txt", src.join("link")).unwrap();
        let archive = tmp.path().join("b.tar.gz");
        let size = pack_dir_gz(&src, &archive).unwrap();
        assert!(size > 0);
        let out = tmp.path().join("out");
        unpack_gz(&archive, &out).unwrap();
        assert_eq!(
            std::fs::read_to_string(out.join("sub/f.txt")).unwrap(),
            "hello"
        );
        assert!(
            std::fs::symlink_metadata(out.join("link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        remove_tree(&out).unwrap();
        remove_tree(&out).unwrap();
        assert!(!out.exists());
    }

    #[tokio::test]
    async fn image_build_edit_and_grow() {
        if !have_e2fsprogs() {
            eprintln!("skipping: e2fsprogs not installed");
            return;
        }
        let tmp = TempDir::new("fc-rootfs");
        let root = tmp.path().join("rootfs-dir");
        std::fs::create_dir_all(root.join("home/agent/work")).unwrap();
        std::fs::write(root.join("home/agent/work/notes.txt"), "keep me").unwrap();
        write_env_file(&root, &BTreeMap::from([("A".to_string(), "1".to_string())])).unwrap();
        let image = tmp.path().join("rootfs.ext4");
        mkfs(&root, &image, 1).await.unwrap();
        assert_eq!(size_gb(&image).unwrap(), 1);
        assert!(allocated_bytes(&image).unwrap() > 0);
        assert!(
            allocated_bytes(&image).unwrap() < i64::try_from(GIB).unwrap() / 4,
            "image file should be sparse"
        );

        assert_eq!(
            cat(&image, "/etc/warren.env").await.unwrap().as_deref(),
            Some("A=1\n")
        );
        assert_eq!(cat(&image, "/etc/nothing").await.unwrap(), None);
        assert!(exists(&image, "/home/agent").await.unwrap());
        assert!(!exists(&image, "/home/nobody").await.unwrap());

        put_file(&image, "/etc/warren.env", "A=2\nB=3\n", 0o600)
            .await
            .unwrap();
        assert_eq!(
            cat(&image, "/etc/warren.env").await.unwrap().as_deref(),
            Some("A=2\nB=3\n")
        );
        put_file(&image, "/etc/fresh.txt", "new", 0o644)
            .await
            .unwrap();
        assert_eq!(
            cat(&image, "/etc/fresh.txt").await.unwrap().as_deref(),
            Some("new")
        );

        let dump = tmp.path().join("dump");
        let home = rdump(&image, "/home/agent", &dump).await.unwrap().unwrap();
        assert_eq!(home, dump.join("agent"));
        assert_eq!(
            std::fs::read_to_string(home.join("work/notes.txt")).unwrap(),
            "keep me"
        );
        assert_eq!(rdump(&image, "/home/nobody", &dump).await.unwrap(), None);

        assert!(grow(&image, 2).await.unwrap());
        assert_eq!(size_gb(&image).unwrap(), 2);
        assert!(!grow(&image, 1).await.unwrap(), "never shrinks");
        assert_eq!(size_gb(&image).unwrap(), 2);
        assert_eq!(
            cat(&image, "/etc/fresh.txt").await.unwrap().as_deref(),
            Some("new")
        );
        assert!(mkfs(&root, &image, 0).await.is_err());
    }
}
