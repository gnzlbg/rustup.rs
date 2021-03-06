//! An interpreter for the rust-installer package format.  Responsible
//! for installing from a directory or tarball to an installation
//! prefix, represented by a `Components` instance.

use crate::dist::component::components::*;
use crate::dist::component::transaction::*;

use crate::dist::temp;
use crate::errors::*;
use crate::utils::notifications::Notification;
use crate::utils::utils;

use std::collections::HashSet;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The current metadata revision used by rust-installer
pub const INSTALLER_VERSION: &str = "3";
pub const VERSION_FILE: &str = "rust-installer-version";

pub trait Package: fmt::Debug {
    fn contains(&self, component: &str, short_name: Option<&str>) -> bool;
    fn install<'a>(
        &self,
        target: &Components,
        component: &str,
        short_name: Option<&str>,
        tx: Transaction<'a>,
    ) -> Result<Transaction<'a>>;
    fn components(&self) -> Vec<String>;
}

#[derive(Debug)]
pub struct DirectoryPackage {
    path: PathBuf,
    components: HashSet<String>,
    copy: bool,
}

impl DirectoryPackage {
    pub fn new(path: PathBuf, copy: bool) -> Result<Self> {
        validate_installer_version(&path)?;

        let content = utils::read_file("package components", &path.join("components"))?;
        let components = content
            .lines()
            .map(std::borrow::ToOwned::to_owned)
            .collect();
        Ok(DirectoryPackage {
            path,
            components,
            copy,
        })
    }
}

fn validate_installer_version(path: &Path) -> Result<()> {
    let file = utils::read_file("installer version", &path.join(VERSION_FILE))?;
    let v = file.trim();
    if v == INSTALLER_VERSION {
        Ok(())
    } else {
        Err(ErrorKind::BadInstallerVersion(v.to_owned()).into())
    }
}

impl Package for DirectoryPackage {
    fn contains(&self, component: &str, short_name: Option<&str>) -> bool {
        self.components.contains(component)
            || if let Some(n) = short_name {
                self.components.contains(n)
            } else {
                false
            }
    }
    fn install<'a>(
        &self,
        target: &Components,
        name: &str,
        short_name: Option<&str>,
        tx: Transaction<'a>,
    ) -> Result<Transaction<'a>> {
        let actual_name = if self.components.contains(name) {
            name
        } else if let Some(n) = short_name {
            n
        } else {
            name
        };

        let root = self.path.join(actual_name);

        let manifest = utils::read_file("package manifest", &root.join("manifest.in"))?;
        let mut builder = target.add(name, tx);

        for l in manifest.lines() {
            let part = ComponentPart::decode(l)
                .ok_or_else(|| ErrorKind::CorruptComponent(name.to_owned()))?;

            let path = part.1;
            let src_path = root.join(&path);

            match &*part.0 {
                "file" => {
                    if self.copy {
                        builder.copy_file(path.clone(), &src_path)?
                    } else {
                        builder.move_file(path.clone(), &src_path)?
                    }
                }
                "dir" => {
                    if self.copy {
                        builder.copy_dir(path.clone(), &src_path)?
                    } else {
                        builder.move_dir(path.clone(), &src_path)?
                    }
                }
                _ => return Err(ErrorKind::CorruptComponent(name.to_owned()).into()),
            }

            set_file_perms(&target.prefix().path().join(path), &src_path)?;
        }

        let tx = builder.finish()?;

        Ok(tx)
    }

    fn components(&self) -> Vec<String> {
        self.components.iter().cloned().collect()
    }
}

// On Unix we need to set up the file permissions correctly so
// binaries are executable and directories readable. This shouldn't be
// necessary: the source files *should* have the right permissions,
// but due to rust-lang/rust#25479 they don't.
#[cfg(unix)]
fn set_file_perms(dest_path: &Path, src_path: &Path) -> Result<()> {
    use std::fs::{self, Metadata};
    use std::os::unix::fs::PermissionsExt;
    use walkdir::WalkDir;

    // Compute whether this entry needs the X bit
    fn needs_x(meta: &Metadata) -> bool {
        meta.is_dir() || // Directories need it
        meta.permissions().mode() & 0o700 == 0o700 // If it is rwx for the user, it gets the X bit
    }

    // By convention, anything in the bin/ directory of the package is a binary
    let is_bin = if let Some(p) = src_path.parent() {
        p.ends_with("bin")
    } else {
        false
    };

    let is_dir = utils::is_directory(dest_path);

    if is_dir {
        // Walk the directory setting everything
        for entry in WalkDir::new(dest_path) {
            let entry = entry.chain_err(|| ErrorKind::ComponentDirPermissionsFailed)?;
            let meta = entry
                .metadata()
                .chain_err(|| ErrorKind::ComponentDirPermissionsFailed)?;

            let mut perm = meta.permissions();
            perm.set_mode(if needs_x(&meta) { 0o755 } else { 0o644 });
            fs::set_permissions(entry.path(), perm)
                .chain_err(|| ErrorKind::ComponentFilePermissionsFailed)?;
        }
    } else {
        let meta =
            fs::metadata(dest_path).chain_err(|| ErrorKind::ComponentFilePermissionsFailed)?;
        let mut perm = meta.permissions();
        perm.set_mode(if is_bin || needs_x(&meta) {
            0o755
        } else {
            0o644
        });
        fs::set_permissions(dest_path, perm)
            .chain_err(|| ErrorKind::ComponentFilePermissionsFailed)?;
    }

    Ok(())
}

#[cfg(windows)]
fn set_file_perms(_dest_path: &Path, _src_path: &Path) -> Result<()> {
    Ok(())
}

#[derive(Debug)]
pub struct TarPackage<'a>(DirectoryPackage, temp::Dir<'a>);

impl<'a> TarPackage<'a> {
    pub fn new<R: Read>(
        stream: R,
        temp_cfg: &'a temp::Cfg,
        notify_handler: Option<&'a dyn Fn(Notification<'_>)>,
    ) -> Result<Self> {
        let temp_dir = temp_cfg.new_directory()?;
        let mut archive = tar::Archive::new(stream);
        // The rust-installer packages unpack to a directory called
        // $pkgname-$version-$target. Skip that directory when
        // unpacking.
        unpack_without_first_dir(&mut archive, &*temp_dir, notify_handler)?;

        Ok(TarPackage(
            DirectoryPackage::new(temp_dir.to_owned(), false)?,
            temp_dir,
        ))
    }
}

#[cfg(windows)]
mod unpacker {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use threadpool;

    use crate::utils::notifications::Notification;

    pub struct Unpacker<'a> {
        n_files: Arc<AtomicUsize>,
        pool: threadpool::ThreadPool,
        notify_handler: Option<&'a dyn Fn(Notification<'_>)>,
    }

    impl<'a> Unpacker<'a> {
        pub fn new(notify_handler: Option<&'a dyn Fn(Notification<'_>)>) -> Self {
            // Defaults to hardware thread count threads; this is suitable for
            // our needs as IO bound operations tend to show up as write latencies
            // rather than close latencies, so we don't need to look at
            // more threads to get more IO dispatched at this stage in the process.
            let pool = threadpool::Builder::new()
                .thread_name("CloseHandle".into())
                .build();
            Unpacker {
                n_files: Arc::new(AtomicUsize::new(0)),
                pool: pool,
                notify_handler: notify_handler,
            }
        }

        pub fn handle(&mut self, unpacked: tar::Unpacked) {
            if let tar::Unpacked::File(f) = unpacked {
                self.n_files.fetch_add(1, Ordering::Relaxed);
                let n_files = self.n_files.clone();
                self.pool.execute(move || {
                    drop(f);
                    n_files.fetch_sub(1, Ordering::Relaxed);
                });
            }
        }
    }

    impl<'a> Drop for Unpacker<'a> {
        fn drop(&mut self) {
            // Some explanation is in order. Even though the tar we are reading from (if
            // any) will have had its FileWithProgress download tracking
            // completed before we hit drop, that is not true if we are unwinding due to a
            // failure, where the logical ownership of the progress bar is
            // ambiguous, and as the tracker itself is abstracted out behind
            // notifications etc we cannot just query for that. So: we assume no
            // more reads of the underlying tar will take place: either the
            // error unwinding will stop reads, or we completed; either way, we
            // notify finished to the tracker to force a reset to zero; we set
            // the units to files, show our progress, and set our units back
            // afterwards. The largest archives today - rust docs - have ~20k
            // items, and the download tracker's progress is confounded with
            // actual handling of data today, we synthesis a data buffer and
            // pretend to have bytes to deliver.
            self.notify_handler
                .map(|handler| handler(Notification::DownloadFinished));
            self.notify_handler
                .map(|handler| handler(Notification::DownloadPushUnits("handles")));
            let mut prev_files = self.n_files.load(Ordering::Relaxed);
            self.notify_handler.map(|handler| {
                handler(Notification::DownloadContentLengthReceived(
                    prev_files as u64,
                ))
            });
            if prev_files > 50 {
                println!("Closing {} deferred file handles", prev_files);
            }
            let buf: Vec<u8> = vec![0; prev_files];
            assert!(32767 > prev_files);
            let mut current_files = prev_files;
            while current_files != 0 {
                use std::thread::sleep;
                sleep(std::time::Duration::from_millis(100));
                prev_files = current_files;
                current_files = self.n_files.load(Ordering::Relaxed);
                let step_count = prev_files - current_files;
                self.notify_handler.map(|handler| {
                    handler(Notification::DownloadDataReceived(&buf[0..step_count]))
                });
            }
            self.pool.join();
            self.notify_handler
                .map(|handler| handler(Notification::DownloadFinished));
            self.notify_handler
                .map(|handler| handler(Notification::DownloadPopUnits));
        }
    }
}

#[cfg(not(windows))]
mod unpacker {
    use crate::utils::notifications::Notification;
    pub struct Unpacker {}
    impl Unpacker {
        pub fn new<'a>(_notify_handler: Option<&'a dyn Fn(Notification<'_>)>) -> Unpacker {
            Unpacker {}
        }
        pub fn handle(&mut self, _unpacked: tar::Unpacked) {}
    }
}

fn unpack_without_first_dir<'a, R: Read>(
    archive: &mut tar::Archive<R>,
    path: &Path,
    notify_handler: Option<&'a dyn Fn(Notification<'_>)>,
) -> Result<()> {
    let mut unpacker = unpacker::Unpacker::new(notify_handler);
    let entries = archive
        .entries()
        .chain_err(|| ErrorKind::ExtractingPackage)?;
    let mut checked_parents: HashSet<PathBuf> = HashSet::new();

    for entry in entries {
        let mut entry = entry.chain_err(|| ErrorKind::ExtractingPackage)?;
        let relpath = {
            let path = entry.path();
            let path = path.chain_err(|| ErrorKind::ExtractingPackage)?;
            path.into_owned()
        };
        let mut components = relpath.components();
        // Throw away the first path component
        components.next();
        let full_path = path.join(&components.as_path());

        // Create the full path to the entry if it does not exist already
        if let Some(parent) = full_path.parent() {
            if !checked_parents.contains(parent) {
                checked_parents.insert(parent.to_owned());
                // It would be nice to optimise this stat out, but the tar could be like so:
                // a/deep/file.txt
                // a/file.txt
                // which would require tracking the segments rather than a simple hash.
                // Until profile shows that one stat per dir is a problem (vs one stat per file)
                // leave till later.

                if !parent.exists() {
                    std::fs::create_dir_all(&parent).chain_err(|| ErrorKind::ExtractingPackage)?
                }
            }
        }
        entry.set_preserve_mtime(false);
        entry
            .unpack(&full_path)
            .map(|unpacked| unpacker.handle(unpacked))
            .chain_err(|| ErrorKind::ExtractingPackage)?;
    }

    Ok(())
}

impl<'a> Package for TarPackage<'a> {
    fn contains(&self, component: &str, short_name: Option<&str>) -> bool {
        self.0.contains(component, short_name)
    }
    fn install<'b>(
        &self,
        target: &Components,
        component: &str,
        short_name: Option<&str>,
        tx: Transaction<'b>,
    ) -> Result<Transaction<'b>> {
        self.0.install(target, component, short_name, tx)
    }
    fn components(&self) -> Vec<String> {
        self.0.components()
    }
}

#[derive(Debug)]
pub struct TarGzPackage<'a>(TarPackage<'a>);

impl<'a> TarGzPackage<'a> {
    pub fn new<R: Read>(
        stream: R,
        temp_cfg: &'a temp::Cfg,
        notify_handler: Option<&'a dyn Fn(Notification<'_>)>,
    ) -> Result<Self> {
        let stream = flate2::read::GzDecoder::new(stream);
        Ok(TarGzPackage(TarPackage::new(
            stream,
            temp_cfg,
            notify_handler,
        )?))
    }
}

impl<'a> Package for TarGzPackage<'a> {
    fn contains(&self, component: &str, short_name: Option<&str>) -> bool {
        self.0.contains(component, short_name)
    }
    fn install<'b>(
        &self,
        target: &Components,
        component: &str,
        short_name: Option<&str>,
        tx: Transaction<'b>,
    ) -> Result<Transaction<'b>> {
        self.0.install(target, component, short_name, tx)
    }
    fn components(&self) -> Vec<String> {
        self.0.components()
    }
}

#[derive(Debug)]
pub struct TarXzPackage<'a>(TarPackage<'a>);

impl<'a> TarXzPackage<'a> {
    pub fn new<R: Read>(
        stream: R,
        temp_cfg: &'a temp::Cfg,
        notify_handler: Option<&'a dyn Fn(Notification<'_>)>,
    ) -> Result<Self> {
        let stream = xz2::read::XzDecoder::new(stream);
        Ok(TarXzPackage(TarPackage::new(
            stream,
            temp_cfg,
            notify_handler,
        )?))
    }
}

impl<'a> Package for TarXzPackage<'a> {
    fn contains(&self, component: &str, short_name: Option<&str>) -> bool {
        self.0.contains(component, short_name)
    }
    fn install<'b>(
        &self,
        target: &Components,
        component: &str,
        short_name: Option<&str>,
        tx: Transaction<'b>,
    ) -> Result<Transaction<'b>> {
        self.0.install(target, component, short_name, tx)
    }
    fn components(&self) -> Vec<String> {
        self.0.components()
    }
}
