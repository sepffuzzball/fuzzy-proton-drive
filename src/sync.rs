use crate::{
    drive::{Cli, Node, ROOT, STOP, safe_name},
    storage,
};
use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{CWD, RenameFlags, renameat_with};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::{self, File, Metadata},
    io::{BufReader, Read},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};

const STAGE: &str = ".fuzzy-proton-stage-";

#[derive(Clone, Deserialize, Serialize)]
pub struct Binding {
    pub account: String,
    pub remote_path: String,
    pub remote_uid: String,
    pub remote_label: String,
    pub local: PathBuf,
    dev: u64,
    ino: u64,
}

impl Binding {
    pub fn new(account: String, node: Node, local: PathBuf, remote_label: String) -> Result<Self> {
        ensure!(node.folder(), "Select a remote folder");
        let local = local
            .canonicalize()
            .context("Select an existing local folder")?;
        ensure!(local.to_str().is_some(), "The local path must be UTF-8");
        let m = local.metadata()?;
        ensure!(
            m.is_dir() && local.parent().is_some(),
            "Select a local directory other than /"
        );
        Ok(Self {
            remote_path: if account == node.uid {
                ROOT.into()
            } else {
                node.path()
            },
            remote_uid: node.uid,
            remote_label,
            local,
            dev: m.dev(),
            ino: m.ino(),
            account,
        })
    }
    pub fn key(&self) -> String {
        let id = serde_json::to_vec(&(
            &self.account,
            &self.remote_uid,
            &self.local,
            self.dev,
            self.ino,
        ))
        .expect("binding identity is serializable");
        blake3::hash(&id).to_hex().to_string()
    }
    fn validate_local(&self) -> Result<()> {
        let m = self.local.symlink_metadata()?;
        ensure!(
            m.is_dir()
                && m.dev() == self.dev
                && m.ino() == self.ino
                && self.local.canonicalize()? == self.local,
            "Local sync folder moved or was replaced; sync paused for this pass"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct RemoteVersion {
    uid: String,
    revision: String,
}

fn version(node: &Node) -> Result<RemoteVersion> {
    Ok(RemoteVersion {
        uid: node.uid.clone(),
        revision: node
            .active_revision
            .as_ref()
            .context("Remote revision unavailable")?
            .uid
            .clone(),
    })
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct Content {
    blake3: String,
    sha1: String,
}

fn content(file: &Path) -> Result<Content> {
    let metadata = file.symlink_metadata()?;
    ensure!(metadata.is_file(), "Not a regular file");
    let before = Stamp::new(&metadata);
    let input = rustix::fs::open(
        file,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?;
    let input = File::from(input);
    ensure!(
        before == Stamp::new(&input.metadata()?),
        "Local file changed before it was opened"
    );
    let mut r = BufReader::new(input);
    let mut blake3 = blake3::Hasher::new();
    let mut sha1 = Sha1::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        ensure!(!STOP.load(Ordering::Relaxed), "Shutting down");
        let n = r.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        blake3.update(&buffer[..n]);
        sha1.update(&buffer[..n]);
    }
    ensure!(
        before == Stamp::new(&file.symlink_metadata()?),
        "Local file changed while being read"
    );
    Ok(Content {
        blake3: blake3.finalize().to_hex().to_string(),
        sha1: format!("{:x}", sha1.finalize()),
    })
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Hash, Serialize)]
struct Stamp(u64, u64, u64, i64, i64, i64, i64);

impl Stamp {
    fn new(m: &Metadata) -> Self {
        Self(
            m.dev(),
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        )
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct Receipt {
    local: Content,
    remote: RemoteVersion,
    local_stamp: Option<Stamp>,
}

#[derive(Default, Deserialize, Serialize)]
struct Journal {
    files: BTreeMap<PathBuf, Receipt>,
    folders: BTreeMap<PathBuf, String>,
    /// Local identity (device, inode) of each synced folder, recorded so a
    /// vanished nested mount (which leaves an empty folder at the same path)
    /// is not mistaken for the deletion of every file inside it.
    #[serde(default)]
    folder_frames: BTreeMap<PathBuf, (u64, u64)>,
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Upload,
    Download,
    Remember,
    None,
    Conflict,
}

fn reconcile(
    base: Option<&Receipt>,
    local: Option<&Content>,
    remote: Option<&RemoteVersion>,
    same_content: bool,
) -> Action {
    use Action::*;
    match (base, local, remote) {
        (_, Option::None, Option::None) => None,
        (Option::None, Some(_), Option::None) => Upload,
        (Option::None, Option::None, Some(_)) => Download,
        (Some(_), Option::None, Some(_)) | (Some(_), Some(_), Option::None) => Conflict,
        (base, Some(l), Some(r)) => {
            if let Some(base) = base {
                if &base.local == l && &base.remote == r {
                    return None;
                }
                if base.remote.uid != r.uid {
                    return Conflict;
                }
                if same_content {
                    return Remember;
                }
                if &base.local == l {
                    return Download;
                }
                if &base.remote == r {
                    return Upload;
                }
                Conflict
            } else if same_content {
                Remember
            } else {
                Conflict
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub uploads: usize,
    pub downloads: usize,
    pub conflicts: usize,
    pub moves: usize,
    pub deletions: usize,
    pub details: Vec<String>,
}

#[derive(Clone, Copy)]
struct LocalFile {
    dev: u64,
    ino: u64,
    len: u64,
}

struct RemoteEntry {
    uid: String,
    revision: RemoteVersion,
}

struct Snapshot {
    /// Local regular files keyed by relative path.
    files: BTreeMap<PathBuf, LocalFile>,
    /// Local folders (including the root as "") keyed by relative path.
    dirs: BTreeMap<PathBuf, (u64, u64)>,
    /// Remote files keyed by relative path.
    remote_files: BTreeMap<PathBuf, RemoteEntry>,
    /// Remote folders keyed by relative path -> UID.
    remote_dirs: BTreeMap<PathBuf, String>,
    /// Reverse lookup: remote UID -> relative path (files and folders).
    remote_uid_path: HashMap<String, PathBuf>,
    /// Reverse lookup: local inode -> relative path. `None` marks an inode
    /// shared by more than one path (a hard link), which we never treat as a
    /// move because it is ambiguous.
    inode_path: HashMap<(u64, u64), Option<PathBuf>>,
}

impl Snapshot {
    fn empty() -> Self {
        Self {
            files: BTreeMap::new(),
            dirs: BTreeMap::new(),
            remote_files: BTreeMap::new(),
            remote_dirs: BTreeMap::new(),
            remote_uid_path: HashMap::new(),
            inode_path: HashMap::new(),
        }
    }
}

struct Pass<'a> {
    cli: &'a Cli,
    binding: &'a Binding,
    state: &'a Path,
    journal: Journal,
    memo: HashMap<Stamp, Content>,
    report: Report,
    snapshot: Snapshot,
    topology_handled: BTreeSet<PathBuf>,
}

pub fn sync(cli: &Cli, binding: &Binding, state: &Path) -> Result<Report> {
    binding.validate_local()?;
    let root = cli.info(ROOT)?;
    ensure!(root.uid == binding.account, "Proton account changed");
    let node = if binding.remote_path == ROOT {
        root
    } else {
        cli.info(&binding.remote_path)?
    };
    ensure!(
        node.folder() && node.uid == binding.remote_uid && node.trash_time.is_none(),
        "Remote sync folder is no longer available"
    );
    let mut pass = Pass {
        cli,
        binding,
        state,
        journal: storage::load(state)
            .context("Cannot read sync journal; not performing transfers")?
            .unwrap_or_default(),
        memo: HashMap::new(),
        report: Report::default(),
        snapshot: Snapshot::empty(),
        topology_handled: BTreeSet::new(),
    };
    // Topology (moves, renames, deletions) is only possible once we hold a
    // baseline from an earlier pass. On the very first pass there is nothing
    // to reconcile structurally, so we skip the extra listing entirely.
    if !pass.journal.files.is_empty() || !pass.journal.folders.is_empty() {
        pass.scan()?;
        pass.apply_topology()?;
    }
    pass.directory(Path::new(""), &binding.remote_path, 0)?;
    Ok(pass.report)
}

impl Pass<'_> {
    fn issue(&mut self, path: &Path, message: &str) {
        self.report.conflicts += 1;
        if self.report.details.len() < 50 {
            self.report
                .details
                .push(format!("{}: {message}", path.display()));
        }
    }

    fn directory(&mut self, relative: &Path, remote_dir: &str, depth: usize) -> Result<()> {
        ensure!(
            depth <= 64,
            "Folder nesting exceeds the 64-level sync limit"
        );
        ensure!(!STOP.load(Ordering::Relaxed), "Shutting down");
        if self.topology_handled.contains(relative) {
            // A folder was moved or removed; do not recreate it or re-download
            // its descendants here. The topology phase already recorded/reported.
            return Ok(());
        }
        let local_dir = self.binding.local.join(relative);
        ensure!(
            local_dir.canonicalize()? == local_dir,
            "Local folder is no longer safe to sync"
        );
        let mut items = BTreeMap::<String, (Option<Metadata>, Option<Node>)>::new();
        for entry in fs::read_dir(&local_dir)? {
            let entry = entry?;
            let Some(name) = entry.file_name().into_string().ok() else {
                self.issue(relative, "Non-UTF-8 name skipped");
                continue;
            };
            if name.starts_with(STAGE) {
                continue;
            }
            if !safe_name(&name) {
                self.issue(&relative.join(name), "Unsupported name");
                continue;
            }
            items.entry(name).or_default().0 = Some(entry.path().symlink_metadata()?);
        }
        for node in self.cli.list(remote_dir)? {
            let Some(name) = node
                .clean_name()
                .filter(|s| safe_name(s) && !s.starts_with(STAGE))
            else {
                // We cannot safely decide which local names are absent in this folder.
                bail!(
                    "Remote folder {} contains an unavailable or unsupported name",
                    relative.display()
                );
            };
            let slot = &mut items.entry(name.into()).or_default().1;
            ensure!(
                slot.is_none(),
                "Duplicate remote name; not syncing this folder"
            );
            *slot = Some(node);
        }
        for (name, (local, remote)) in items {
            ensure!(!STOP.load(Ordering::Relaxed), "Shutting down");
            let relative = relative.join(&name);
            if let Err(err) = self.item(&relative, remote_dir, local, remote, depth) {
                self.issue(&relative, &format!("{err:#}"));
            }
        }
        Ok(())
    }

    fn item(
        &mut self,
        relative: &Path,
        remote_dir: &str,
        local: Option<Metadata>,
        remote: Option<Node>,
        depth: usize,
    ) -> Result<()> {
        if self.topology_handled.contains(relative) {
            // Structural change already handled (or deliberately preserved):
            // do not upload/download a duplicate or delete a moved file here.
            return Ok(());
        }
        let name = relative
            .file_name()
            .and_then(|n| n.to_str())
            .context("Invalid local name")?;
        let path = self.binding.local.join(relative);
        let is_local_folder = local.as_ref().is_some_and(Metadata::is_dir);
        let is_remote_folder = remote.as_ref().is_some_and(Node::folder);
        ensure!(
            local.as_ref().is_none_or(|m| m.is_file() || m.is_dir()),
            "Symbolic link or special file skipped"
        );
        if is_local_folder || is_remote_folder {
            ensure!(
                !self.journal.files.contains_key(relative),
                "Previously synced file became a folder"
            );
            ensure!(
                (local.is_none() || is_local_folder) && (remote.is_none() || is_remote_folder),
                "File/folder conflict; retained both"
            );
            if let Some(uid) = self.journal.folders.get(relative) {
                ensure!(
                    is_local_folder && remote.as_ref().is_some_and(|n| &n.uid == uid),
                    "Folder removed or replaced on one side; retained the other side"
                );
            }
            let remote = match remote {
                Some(node) => node,
                None => {
                    self.preflight(&path, remote_dir, name, None)?;
                    self.cli.mkdir(remote_dir, name)?
                }
            };
            if !is_local_folder {
                self.preflight(&path, remote_dir, name, Some(&remote))?;
                fs::create_dir(&path)?;
            }
            if self.journal.folders.get(relative) != Some(&remote.uid) {
                self.journal
                    .folders
                    .insert(relative.into(), remote.uid.clone());
                storage::save(self.state, &self.journal)?;
            }
            // Record the local folder identity once. It is intentionally never
            // overwritten: a folder that is later replaced (for example by a
            // missing nested mount) keeps its original identity so files inside
            // it are never mistaken for deleted.
            if !self.journal.folder_frames.contains_key(relative) {
                let m = fs::symlink_metadata(&path)?;
                self.journal
                    .folder_frames
                    .insert(relative.into(), (m.dev(), m.ino()));
                storage::save(self.state, &self.journal)?;
            }
            return self.directory(relative, &remote.path(), depth + 1);
        }
        ensure!(
            !self.journal.folders.contains_key(relative),
            "Previously synced folder became a file"
        );
        ensure!(
            remote.as_ref().is_none_or(|r| r.kind == "file"),
            "Unsupported remote item"
        );
        let stamp = local.as_ref().map(Stamp::new);
        let local_content = stamp
            .map(|stamp| {
                if let Some(receipt) = self.journal.files.get(relative)
                    && receipt.local_stamp == Some(stamp)
                {
                    return Ok(receipt.local.clone());
                }
                if let Some(found) = self.memo.get(&stamp) {
                    return Ok::<_, anyhow::Error>(found.clone());
                }
                let value = content(&path)?;
                self.memo.insert(stamp, value.clone());
                Ok(value)
            })
            .transpose()?;
        let remote_version = remote.as_ref().map(version).transpose()?;
        let same = local_content
            .as_ref()
            .zip(remote.as_ref())
            .is_some_and(|(l, r)| digest_matches(l, r));
        let decision = reconcile(
            self.journal.files.get(relative),
            local_content.as_ref(),
            remote_version.as_ref(),
            same,
        );
        let receipt = match decision {
            Action::None => {
                if let Some(receipt) = self.journal.files.get_mut(relative)
                    && receipt.local_stamp != stamp
                {
                    receipt.local_stamp = stamp;
                    storage::save(self.state, &self.journal)?;
                }
                return Ok(());
            }
            Action::Conflict => {
                bail!("One side was removed or both sides changed; kept existing copies")
            }
            Action::Remember => Receipt {
                local: local_content.unwrap(),
                remote: remote_version.unwrap(),
                local_stamp: stamp,
            },
            Action::Upload => {
                let local_content = local_content.unwrap();
                let temp = tempfile::Builder::new()
                    .prefix(STAGE)
                    .tempdir_in(path.parent().unwrap())?;
                let staged = temp.path().join(name);
                fs::copy(&path, &staged)?;
                ensure!(
                    content(&staged)? == local_content,
                    "Local file changed before upload"
                );
                self.preflight(&path, remote_dir, name, remote.as_ref())?;
                let transferred = self.cli.upload(&staged, remote_dir, remote.is_some())?;
                let current = self
                    .find(remote_dir, name)?
                    .context("Uploaded file is not visible yet")?;
                ensure!(
                    digest_matches(&local_content, &current),
                    "Cloud content does not match upload; no receipt recorded"
                );
                if transferred {
                    self.report.uploads += 1;
                }
                Receipt {
                    local: local_content,
                    remote: version(&current)?,
                    local_stamp: stamp,
                }
            }
            Action::Download => {
                let remote = remote.unwrap();
                let temp = tempfile::Builder::new()
                    .prefix(STAGE)
                    .tempdir_in(path.parent().unwrap())?;
                let downloaded = self.cli.download(&remote, temp.path())?;
                let downloaded_content = content(&downloaded)?;
                if expected_digest(&remote).is_some() {
                    ensure!(
                        digest_matches(&downloaded_content, &remote),
                        "Downloaded content does not match expected revision"
                    );
                }
                self.preflight(&path, remote_dir, name, Some(&remote))?;
                if let Some(expected) = local_content {
                    ensure!(
                        content(&path)? == expected,
                        "Local file changed during download"
                    );
                    File::open(&downloaded)?.sync_all()?;
                    fs::rename(downloaded, &path)?;
                } else {
                    File::open(&downloaded)?.sync_all()?;
                    fs::hard_link(downloaded, &path).context("Local file appeared during download, or filesystem cannot install it safely")?;
                }
                File::open(path.parent().unwrap())?.sync_all()?;
                self.report.downloads += 1;
                // Derive the stamp from the installed inode only if it still contains
                // the expected bytes; an editor may have saved immediately after rename.
                Receipt {
                    local: downloaded_content,
                    remote: remote_version.unwrap(),
                    local_stamp: None,
                }
            }
        };
        self.journal.files.insert(relative.into(), receipt);
        storage::save(self.state, &self.journal)
    }

    fn find(&self, parent: &str, name: &str) -> Result<Option<Node>> {
        let mut found = None;
        for node in self.cli.list(parent)? {
            let n = node.clean_name().context("Unavailable remote name")?;
            if n == name {
                ensure!(found.is_none(), "Duplicate remote name");
                found = Some(node);
            }
        }
        Ok(found)
    }

    /// Walk the whole local and remote trees, collecting everything needed to
    /// decide structural changes. Any incomplete/unsupported scan bails here,
    /// before any topology mutation is attempted.
    fn scan(&mut self) -> Result<()> {
        self.scan_local(Path::new(""))?;
        self.scan_remote(Path::new(""), &self.binding.remote_path)?;
        Ok(())
    }

    fn scan_local(&mut self, relative: &Path) -> Result<()> {
        ensure!(
            relative.components().count() <= 64,
            "Folder nesting exceeds the 64-level sync limit"
        );
        ensure!(!STOP.load(Ordering::Relaxed), "Shutting down");
        let dir = self.binding.local.join(relative);
        ensure!(
            dir.canonicalize()? == dir,
            "Local folder is no longer safe to sync"
        );
        let mut entries = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("Non-UTF-8 name makes sync snapshot incomplete"))?;
            if name.starts_with(STAGE) {
                continue;
            }
            ensure!(
                safe_name(&name),
                "Unsupported name makes sync snapshot incomplete"
            );
            entries.push((name, entry.path()));
        }
        for (name, path) in entries {
            let rel = relative.join(&name);
            let m = path.symlink_metadata()?;
            if m.is_dir() {
                self.snapshot.dirs.insert(rel.clone(), (m.dev(), m.ino()));
                self.scan_local(&rel)?;
            } else if m.is_file() {
                let lf = LocalFile {
                    dev: m.dev(),
                    ino: m.ino(),
                    len: m.len(),
                };
                self.snapshot.files.insert(rel.clone(), lf);
                let key = (lf.dev, lf.ino);
                self.snapshot
                    .inode_path
                    .entry(key)
                    .and_modify(|value| *value = None)
                    .or_insert_with(|| (m.nlink() == 1).then_some(rel));
            }
            // Symbolic links and special files are not part of the topology
            // (the normal traversal already reports and skips them).
        }
        Ok(())
    }

    fn scan_remote(&mut self, relative: &Path, remote_dir: &str) -> Result<()> {
        ensure!(
            relative.components().count() <= 64,
            "Folder nesting exceeds the 64-level sync limit"
        );
        ensure!(!STOP.load(Ordering::Relaxed), "Shutting down");
        for node in self.cli.list(remote_dir)? {
            let Some(name) = node
                .clean_name()
                .filter(|s| safe_name(s) && !s.starts_with(STAGE))
            else {
                bail!(
                    "Remote folder {} contains an unavailable or unsupported name",
                    relative.display()
                );
            };
            let rel = relative.join(name);
            ensure!(
                !self.snapshot.remote_dirs.contains_key(&rel)
                    && !self.snapshot.remote_files.contains_key(&rel)
                    && !self.snapshot.remote_uid_path.contains_key(&node.uid),
                "Duplicate name or UID makes the cloud snapshot incomplete"
            );
            if node.folder() {
                self.snapshot
                    .remote_dirs
                    .insert(rel.clone(), node.uid.clone());
                self.snapshot
                    .remote_uid_path
                    .insert(node.uid.clone(), rel.clone());
                self.scan_remote(&rel, &node.path())?;
            } else {
                ensure!(
                    node.kind == "file",
                    "Unsupported remote item with an unknown type"
                );
                let revision = version(&node)?;
                self.snapshot.remote_files.insert(
                    rel.clone(),
                    RemoteEntry {
                        uid: node.uid.clone(),
                        revision,
                    },
                );
                self.snapshot
                    .remote_uid_path
                    .insert(node.uid.clone(), rel.clone());
            }
        }
        Ok(())
    }

    /// Apply structural changes (moves, renames, deletions) based on the full
    /// snapshot and the prior baseline. Fails closed on any ambiguity: copies
    /// are preserved rather than duplicated or dropped.
    fn apply_topology(&mut self) -> Result<()> {
        let paths: Vec<PathBuf> = self.journal.files.keys().cloned().collect();
        for path in paths {
            self.remote_topology(&path)?;
        }
        let paths: Vec<PathBuf> = self.journal.files.keys().cloned().collect();
        for path in paths {
            self.local_topology(&path)?;
        }
        Ok(())
    }

    /// Handle a change observed on the remote side: the journaled UID either
    /// moved to a new path (replicate the move locally) or left the pair
    /// (trash the local copy only if info confirms the remote in trash).
    fn remote_topology(&mut self, path: &Path) -> Result<()> {
        let Some(receipt) = self.journal.files.get(path).cloned() else {
            return Ok(());
        };
        let current = self.snapshot.remote_files.get(path);
        let still_there = current.is_some_and(|r| r.uid == receipt.remote.uid);
        if still_there {
            return Ok(());
        }
        // The journaled UID may have moved to another path within the pair.
        if let Some(q) = self
            .snapshot
            .remote_uid_path
            .get(&receipt.remote.uid)
            .filter(|q| *q != path)
            .cloned()
            && self.snapshot.remote_files.contains_key(&q)
        {
            return self.replicate_remote_move(path, &q, &receipt);
        }
        // Otherwise the remote node is gone from the pair. Confirm via `info`
        // that it is in Trash before touching
        // the local copy.
        match self.cli.info(&remote_path_for(&receipt.remote.uid)) {
            Ok(node)
                if node.uid == receipt.remote.uid
                    && node.trash_time.is_some()
                    && version(&node)? == receipt.remote
                    && current.is_none() =>
            {
                self.delete_local_copy(path, &receipt)?;
            }
            Ok(_) => {
                self.topology_handled.insert(path.into());
                if let Some(moved) = self.local_move_target(path, &receipt) {
                    self.topology_handled.insert(moved);
                }
                self.issue(
                    path,
                    "Cloud file moved, changed or was replaced; local copy retained",
                );
            }
            Err(_) => {
                self.topology_handled.insert(path.into());
                if let Some(moved) = self.local_move_target(path, &receipt) {
                    self.topology_handled.insert(moved);
                }
                self.issue(
                    path,
                    "Could not confirm cloud deletion; local copy retained",
                );
            }
        }
        Ok(())
    }

    fn replicate_remote_move(&mut self, from: &Path, to: &Path, receipt: &Receipt) -> Result<()> {
        self.topology_handled.insert(from.into());
        self.topology_handled.insert(to.into());
        let source = self.binding.local.join(from);
        let target = self.binding.local.join(to);
        let remote = self
            .snapshot
            .remote_files
            .get(to)
            .context("Moved remote file unavailable")?;
        let unchanged_remote = remote.revision == receipt.remote;
        let moved_together = self.local_move_target(from, receipt).as_deref() == Some(to)
            && fs::symlink_metadata(&source)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound);
        if unchanged_remote
            && moved_together
            && content(&target).is_ok_and(|v| v == receipt.local)
            && !self.journal.files.contains_key(to)
        {
            let mut receipt = receipt.clone();
            receipt.local_stamp = Some(Stamp::new(&target.symlink_metadata()?));
            self.journal.files.remove(from);
            self.journal.files.insert(to.into(), receipt);
            storage::save(self.state, &self.journal)?;
            self.report.moves += 1;
            return Ok(());
        }
        if !unchanged_remote
            || self.snapshot.remote_files.contains_key(from)
            || self.snapshot.remote_dirs.contains_key(from)
            || self.journal.files.contains_key(to)
            || !content(&source).is_ok_and(|v| v == receipt.local)
            || !self.safe_existing_local_parent(from)
            || !self.safe_destination_ancestors(to)
        {
            if let Some(path) = self.local_move_target(from, receipt) {
                self.topology_handled.insert(path);
            }
            self.issue(
                from,
                "Cloud move conflicts with another change; kept both copies",
            );
            return Ok(());
        }
        // Destination must not be occupied by a different local file.
        let occupied = !missing(&target)?;
        if occupied {
            self.issue(
                from,
                "Remote file moved to an occupied local name; kept both",
            );
            self.topology_handled.insert(from.into());
            self.topology_handled.insert(to.into());
            return Ok(());
        }
        // Create the destination folder if it does not exist yet.
        self.binding.validate_local()?;
        let remote_parent = self
            .snapshot_parent(to)
            .context("Moved cloud parent missing")?;
        let fresh = self.cli.info(&remote_path_for(&receipt.remote.uid))?;
        ensure!(
            version(&fresh)? == receipt.remote
                && fresh.trash_time.is_none()
                && fresh.parent_uid.as_deref() == Some(remote_parent)
                && fresh.clean_name() == to.file_name().and_then(|n| n.to_str()),
            "Cloud file changed during move; kept both copies"
        );
        if let Some(parent) = to.parent() {
            fs::create_dir_all(self.binding.local.join(parent))?;
        }
        ensure!(
            self.safe_existing_local_parent(from)
                && self.safe_existing_local_parent(to)
                && content(&source)? == receipt.local,
            "Local file or parent changed during move"
        );
        self.move_local(from, to)?;
        // Reflect the move in the working snapshot and journal. The remote
        // revision at `to` and the baseline content were both verified.
        self.snapshot.files.insert(
            to.into(),
            LocalFile {
                dev: target.symlink_metadata()?.dev(),
                ino: target.symlink_metadata()?.ino(),
                len: target.symlink_metadata()?.len(),
            },
        );
        self.snapshot.files.remove(from);
        let revision = self
            .snapshot
            .remote_files
            .get(to)
            .map(|r| r.revision.clone())
            .unwrap_or_else(|| receipt.remote.clone());
        let remote_uid = self
            .snapshot
            .remote_files
            .get(to)
            .map(|r| r.uid.clone())
            .unwrap_or_else(|| receipt.remote.uid.clone());
        let new_receipt = Receipt {
            local: receipt.local.clone(),
            remote: RemoteVersion {
                uid: remote_uid,
                revision: revision.revision,
            },
            local_stamp: None,
        };
        self.journal.files.remove(from);
        self.journal.files.insert(to.into(), new_receipt);
        self.report.moves += 1;
        storage::save(self.state, &self.journal)?;
        Ok(())
    }

    /// Move a single local file (no overwrite), leaving both copies on a
    /// conflict so we never silently drop data.
    fn move_local(&self, from: &Path, to: &Path) -> Result<()> {
        let source = self.binding.local.join(from);
        let target = self.binding.local.join(to);
        let moved = move_no_replace(&source, &target)?;
        if !moved {
            bail!(
                "Local move blocked by an existing file at {}; kept both",
                target.display()
            );
        }
        File::open(
            target
                .parent()
                .context("Missing local destination parent")?,
        )?
        .sync_all()?;
        Ok(())
    }

    fn delete_local_copy(&mut self, path: &Path, receipt: &Receipt) -> Result<()> {
        self.topology_handled.insert(path.into());
        let local = self.binding.local.join(path);
        if let Some(stamp) = receipt.local_stamp
            && let Some(moved) = self.snapshot.inode_path.get(&(stamp.0, stamp.1))
        {
            if let Some(moved) = moved
                && moved != path
            {
                self.topology_handled.insert(moved.clone());
                self.issue(
                    path,
                    "Local move conflicts with cloud deletion; kept local copy",
                );
                return Ok(());
            } else if moved.is_none() {
                self.issue(path, "Local inode has multiple names; kept local copy");
                return Ok(());
            }
        }
        if missing(&local)? {
            if self.safe_existing_local_parent(path) {
                self.journal.files.remove(path);
                storage::save(self.state, &self.journal)?;
            }
            return Ok(());
        }
        // Revalidate immediately before mutation: the local copy must still be
        // the baseline content, and the binding must still be valid.
        self.binding.validate_local()?;
        let stamp = Stamp::new(&local.symlink_metadata()?);
        let unchanged = self.safe_existing_local_parent(path)
            && match content(&local) {
                Ok(c) => c == receipt.local,
                Err(_) => false,
            };
        if !unchanged {
            self.issue(
                path,
                "Local file changed before cloud deletion; kept local copy",
            );
            return Ok(());
        }
        ensure!(
            Stamp::new(&local.symlink_metadata()?) == stamp,
            "Local file changed before trash"
        );
        self.cli.trash_local(&local)?;
        self.journal.files.remove(path);
        self.report.deletions += 1;
        storage::save(self.state, &self.journal)?;
        Ok(())
    }

    /// Handle a change observed on the local side: the file either moved to a
    /// new local path (replicate to remote) or was deleted (trash remote only
    /// if the parent is unchanged and we hold a usable local inode).
    fn local_topology(&mut self, path: &Path) -> Result<()> {
        if self.topology_handled.contains(path) {
            return Ok(());
        }
        let Some(receipt) = self.journal.files.get(path).cloned() else {
            return Ok(());
        };
        let local_present = self.snapshot.files.contains_key(path);
        let remote_present = self.snapshot.remote_files.contains_key(path);
        if local_present && remote_present {
            return Ok(());
        }
        if !local_present && !remote_present {
            // Removed on both sides; drop the baseline.
            self.journal.files.remove(path);
            storage::save(self.state, &self.journal)?;
            return Ok(());
        }
        if !local_present && remote_present {
            if !missing(&self.binding.local.join(path))? {
                self.topology_handled.insert(path.into());
                self.issue(
                    path,
                    "Local path was replaced by another kind of item; kept cloud copy",
                );
                return Ok(());
            }
            // The remote copy is intact, so this is a local-origin removal.
            // First check whether the file merely moved to another local path.
            if let Some(q) = self.local_move_target(path, &receipt) {
                return self.replicate_local_move(path, &q, &receipt);
            }
            if let Some(stamp) = receipt.local_stamp
                && self.snapshot.inode_path.contains_key(&(stamp.0, stamp.1))
            {
                self.topology_handled.insert(path.into());
                let ambiguous: Vec<_> = self
                    .snapshot
                    .files
                    .iter()
                    .filter(|(_, file)| (file.dev, file.ino) == (stamp.0, stamp.1))
                    .map(|(path, _)| path.clone())
                    .collect();
                self.topology_handled.extend(ambiguous);
                self.issue(path, "Local file has ambiguous hard links; kept cloud copy");
                return Ok(());
            }
            return self.delete_remote_copy(path, &receipt);
        }
        Ok(())
    }

    fn local_move_target(&self, path: &Path, receipt: &Receipt) -> Option<PathBuf> {
        let stamp = receipt.local_stamp?;
        let inode = (stamp.0, stamp.1);
        let q = self.snapshot.inode_path.get(&inode).cloned().flatten()?;
        (q != path).then_some(q)
    }

    fn replicate_local_move(&mut self, from: &Path, to: &Path, receipt: &Receipt) -> Result<()> {
        self.topology_handled.insert(from.into());
        self.topology_handled.insert(to.into());
        let local_target = self.binding.local.join(to);
        let stamp = Stamp::new(&local_target.symlink_metadata()?);
        // Only safe when the remote source is still the baseline.
        let remote_source_changed = self
            .snapshot
            .remote_files
            .get(from)
            .is_some_and(|r| r.uid != receipt.remote.uid || r.revision != receipt.remote);
        let dest_occupied = self.snapshot.remote_files.contains_key(to)
            || self.snapshot.remote_dirs.contains_key(to)
            || self.journal.files.contains_key(to);
        if remote_source_changed
            || dest_occupied
            || !self.safe_existing_local_parent(from)
            || !self.safe_existing_local_parent(to)
            || !self.remote_ancestors_unchanged(from)
            || !self.remote_ancestors_unchanged(to)
            || !content(&local_target).is_ok_and(|c| c == receipt.local)
        {
            self.issue(
                from,
                "Local move blocked by a changed or occupied remote target; kept both",
            );
            self.topology_handled.insert(from.into());
            self.topology_handled.insert(to.into());
            return Ok(());
        }
        let parent = to.parent().unwrap_or(Path::new(""));
        let from_parent = from.parent().unwrap_or(Path::new(""));
        let name = to
            .file_name()
            .context("Missing destination file name")?
            .to_str()
            .context("Non-UTF-8 destination name")?
            .to_owned();
        let remote_source_path = format!("{ROOT}/{}", receipt.remote.uid);
        let source_parent = self
            .snapshot_parent(from)
            .context("Cloud source parent missing")?
            .to_owned();
        let destination = self.ensure_remote_dir(parent)?;
        let fresh = self.cli.info(&remote_source_path)?;
        ensure!(
            version(&fresh)? == receipt.remote
                && fresh.trash_time.is_none()
                && fresh.parent_uid.as_deref() == Some(source_parent.as_str())
                && fresh.clean_name() == from.file_name().and_then(|n| n.to_str()),
            "Cloud source changed during move"
        );
        ensure!(
            self.find(&destination, &name)?.is_none(),
            "Cloud destination became occupied"
        );
        self.binding.validate_local()?;
        ensure!(
            missing(&self.binding.local.join(from))? && content(&local_target)? == receipt.local,
            "Local move changed during sync"
        );
        if from_parent == parent {
            // In-place rename.
            self.cli
                .rename(&remote_source_path, &name, &receipt.remote.uid)?;
        } else {
            // Cross-folder move; ensure the counterpart destination folder
            // exists remotely first.
            let old_name = from
                .file_name()
                .and_then(|n| n.to_str())
                .context("Invalid source name")?;
            ensure!(
                self.find(&destination, old_name)?.is_none(),
                "Intermediate cloud destination is occupied"
            );
            self.cli
                .move_node(&remote_source_path, &destination, &receipt.remote.uid)?;
            if from.file_name() != to.file_name() {
                self.cli.rename(
                    &format!("{ROOT}/{}", receipt.remote.uid),
                    &name,
                    &receipt.remote.uid,
                )?;
            }
        }
        // The remote node's UID is stable; refresh its revision and reflect the
        // move into the working snapshot and journal.
        let moved = self.cli.info(&format!("{ROOT}/{}", receipt.remote.uid))?;
        let revision = version(&moved)?;
        ensure!(
            revision == receipt.remote
                && moved.clean_name() == Some(name.as_str())
                && moved.parent_uid.as_deref() == self.snapshot_parent(to),
            "Cloud file changed during move; no baseline recorded"
        );
        self.snapshot.remote_files.remove(from);
        self.snapshot.remote_files.insert(
            to.into(),
            RemoteEntry {
                uid: moved.uid.clone(),
                revision: revision.clone(),
            },
        );
        self.snapshot
            .remote_uid_path
            .insert(moved.uid.clone(), to.into());
        let new_receipt = Receipt {
            local: receipt.local.clone(),
            remote: revision,
            local_stamp: (local_target
                .symlink_metadata()
                .is_ok_and(|m| Stamp::new(&m) == stamp))
            .then_some(stamp),
        };
        self.journal.files.remove(from);
        self.journal.files.insert(to.into(), new_receipt);
        self.report.moves += 1;
        storage::save(self.state, &self.journal)?;
        Ok(())
    }

    /// Return the remote global UID path for a folder, creating any missing
    /// intermediate folders (and recording them) along the way.
    fn ensure_remote_dir(&mut self, relative_parent: &Path) -> Result<String> {
        if relative_parent.as_os_str().is_empty() {
            return Ok(self.binding.remote_path.clone());
        }
        if let Some(uid) = self.snapshot.remote_dirs.get(relative_parent) {
            return Ok(format!("{ROOT}/{uid}"));
        }
        let parent = relative_parent.parent().unwrap_or(Path::new(""));
        let name = relative_parent
            .file_name()
            .context("Missing folder name")?
            .to_str()
            .context("Non-UTF-8 folder name")?;
        let parent_path = self.ensure_remote_dir(parent)?;
        let node = self.cli.mkdir(&parent_path, name)?;
        self.snapshot
            .remote_dirs
            .insert(relative_parent.into(), node.uid.clone());
        self.snapshot
            .remote_uid_path
            .insert(node.uid.clone(), relative_parent.into());
        self.journal
            .folders
            .insert(relative_parent.into(), node.uid.clone());
        let m = self
            .binding
            .local
            .join(relative_parent)
            .symlink_metadata()?;
        self.journal
            .folder_frames
            .insert(relative_parent.into(), (m.dev(), m.ino()));
        storage::save(self.state, &self.journal)?;
        Ok(format!("{ROOT}/{}", node.uid))
    }

    /// Trash the remote baseline when the local copy is genuinely gone and the
    /// parent directory is unchanged (so a vanished mount is not mistaken for
    /// the deletion of every synced file inside it).
    fn delete_remote_copy(&mut self, path: &Path, receipt: &Receipt) -> Result<()> {
        // Fail closed if we cannot prove deletion, so an edited/moved copy is
        // never dropped. The traversal reports these as conflicts and keeps the
        // cloud copy.
        let Some(stamp) = receipt.local_stamp else {
            return Ok(());
        };
        if !self.local_parent_unchanged(path, stamp) || !self.remote_ancestors_unchanged(path) {
            return Ok(());
        }
        // A move across filesystems can change the inode. Matching bytes at a
        // new path suggest a copy-or-move; don't guess which by trashing.
        let candidates: Vec<_> = self
            .snapshot
            .files
            .iter()
            .filter(|(candidate, file)| {
                file.len == stamp.2 && !self.journal.files.contains_key(*candidate)
            })
            .filter(|(candidate, _)| {
                !content(&self.binding.local.join(candidate))
                    .is_ok_and(|bytes| bytes != receipt.local)
            })
            .map(|(candidate, _)| candidate.clone())
            .collect();
        if !candidates.is_empty() {
            self.topology_handled.insert(path.into());
            self.topology_handled.extend(candidates);
            self.issue(
                path,
                "Possible copy or move without a stable inode; kept cloud copy",
            );
            return Ok(());
        }
        // Revalidate immediately before mutation: the remote copy must still be
        // baseline and the binding must still be valid.
        self.binding.validate_local()?;
        let remote_path = format!("{ROOT}/{}", receipt.remote.uid);
        match self.cli.info(&remote_path) {
            Ok(node) if node.trash_time.is_none() => {
                // Only proceed if the node did not already change/move.
                if node.active_revision.as_ref().map(|r| r.uid.as_str())
                    != Some(receipt.remote.revision.as_str())
                    || node.uid != receipt.remote.uid
                    || node.parent_uid.as_deref() != self.snapshot_parent(path)
                    || node.clean_name() != path.file_name().and_then(|n| n.to_str())
                    || !self.local_parent_unchanged(path, stamp)
                    || !missing(&self.binding.local.join(path))?
                {
                    return Ok(());
                }
                self.cli.trash(&remote_path, &receipt.remote.uid)?;
                self.journal.files.remove(path);
                self.report.deletions += 1;
                storage::save(self.state, &self.journal)?;
            }
            Ok(_) => {
                // Already trashed; just drop the record.
                self.journal.files.remove(path);
                storage::save(self.state, &self.journal)?;
            }
            Err(_) => {}
        }
        Ok(())
    }

    /// The immediate parent of a file must still exist and (where we recorded
    /// an identity) match the recorded device/inode. Otherwise a vanished
    /// nested mount could look like deleted files.
    fn local_parent_unchanged(&self, path: &Path, file: Stamp) -> bool {
        let parent = path.parent().unwrap_or(Path::new(""));
        let frame = if parent.as_os_str().is_empty() {
            (self.binding.dev, self.binding.ino)
        } else {
            match self.snapshot.dirs.get(parent) {
                Some(frame) => *frame,
                None => return false,
            }
        };
        match self.journal.folder_frames.get(parent) {
            Some(recorded) => {
                *recorded == frame && file.0 == frame.0 && self.safe_existing_local_parent(path)
            }
            None => {
                parent.as_os_str().is_empty()
                    && file.0 == frame.0
                    && self.safe_existing_local_parent(path)
            }
        }
    }

    fn snapshot_parent(&self, path: &Path) -> Option<&str> {
        let parent = path.parent()?;
        if parent.as_os_str().is_empty() {
            Some(&self.binding.remote_uid)
        } else {
            self.snapshot.remote_dirs.get(parent).map(String::as_str)
        }
    }

    fn remote_ancestors_unchanged(&self, path: &Path) -> bool {
        path.ancestors().skip(1).all(|ancestor| {
            self.journal
                .folders
                .get(ancestor)
                .is_none_or(|expected| self.snapshot.remote_dirs.get(ancestor) == Some(expected))
        })
    }

    fn safe_destination_ancestors(&self, path: &Path) -> bool {
        self.remote_ancestors_unchanged(path)
            && path.ancestors().skip(1).all(|ancestor| {
                let local = self.binding.local.join(ancestor);
                match local.symlink_metadata() {
                    Ok(m) => {
                        m.is_dir()
                            && self
                                .journal
                                .folder_frames
                                .get(ancestor)
                                .is_none_or(|frame| *frame == (m.dev(), m.ino()))
                    }
                    Err(e) => {
                        e.kind() == std::io::ErrorKind::NotFound
                            && !self.journal.folder_frames.contains_key(ancestor)
                    }
                }
            })
    }

    fn safe_existing_local_parent(&self, path: &Path) -> bool {
        self.binding.validate_local().is_ok()
            && self.safe_destination_ancestors(path)
            && path.parent().is_some_and(|parent| {
                let local = self.binding.local.join(parent);
                local.canonicalize().is_ok_and(|p| p == local)
            })
    }

    fn preflight(
        &self,
        path: &Path,
        remote_dir: &str,
        name: &str,
        remote: Option<&Node>,
    ) -> Result<()> {
        let parent = path.parent().context("Missing local parent")?;
        let actual = self.find(remote_dir, name)?;
        ensure!(
            match (remote, actual.as_ref()) {
                (None, None) => true,
                (Some(a), Some(b)) if a.folder() && b.folder() => a.uid == b.uid,
                (Some(a), Some(b)) => version(a)? == version(b)?,
                _ => false,
            },
            "Remote item changed during sync; not overwriting it"
        );
        self.binding.validate_local()?;
        ensure!(
            parent.canonicalize()? == parent,
            "Local parent was replaced"
        );
        Ok(())
    }
}

fn remote_path_for(uid: &str) -> String {
    format!("{ROOT}/{uid}")
}

fn missing(path: &Path) -> Result<bool> {
    match path.symlink_metadata() {
        Ok(_) => Ok(false),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(err) => Err(err.into()),
    }
}

fn expected_digest(node: &Node) -> Option<&str> {
    node.active_revision
        .as_ref()?
        .claimed_digests
        .as_ref()?
        .sha1
        .as_deref()
}

fn digest_matches(bytes: &Content, node: &Node) -> bool {
    expected_digest(node).is_some_and(|digest| digest.eq_ignore_ascii_case(&bytes.sha1))
}

/// Move `from` to `to` without overwriting an existing `to`. Returns `true` if
/// the file was moved and `false` if the destination is already occupied.
/// Fails closed (leaving both copies) on cross-device or unsupported
/// filesystems.
fn move_no_replace(from: &Path, to: &Path) -> Result<bool> {
    match renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(err) => {
            bail!(
                "Local move failed without overwriting {}: {err}; kept both copies",
                to.display()
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn local(s: &str) -> Content {
        Content {
            blake3: s.into(),
            sha1: s.into(),
        }
    }
    fn remote(s: &str) -> RemoteVersion {
        RemoteVersion {
            uid: "file".into(),
            revision: s.into(),
        }
    }

    #[test]
    fn sync_decisions_preserve_conflicts_and_deletions() {
        let a = local("a");
        let b = local("b");
        let r1 = remote("1");
        let r2 = remote("2");
        let base = Receipt {
            local: a.clone(),
            remote: r1.clone(),
            local_stamp: None,
        };
        assert_eq!(reconcile(None, Some(&a), None, false), Action::Upload);
        assert_eq!(reconcile(None, None, Some(&r1), false), Action::Download);
        assert_eq!(
            reconcile(None, Some(&a), Some(&r1), false),
            Action::Conflict
        );
        assert_eq!(
            reconcile(Some(&base), Some(&a), Some(&r1), false),
            Action::None
        );
        assert_eq!(
            reconcile(Some(&base), Some(&b), Some(&r1), false),
            Action::Upload
        );
        assert_eq!(
            reconcile(Some(&base), Some(&a), Some(&r2), false),
            Action::Download
        );
        assert_eq!(
            reconcile(Some(&base), Some(&b), Some(&r2), false),
            Action::Conflict
        );
        assert_eq!(
            reconcile(Some(&base), Some(&b), Some(&r2), true),
            Action::Remember
        );
        assert_eq!(
            reconcile(Some(&base), None, Some(&r2), false),
            Action::Conflict
        );
        assert_eq!(
            reconcile(Some(&base), Some(&a), None, false),
            Action::Conflict
        );
    }

    #[test]
    fn cli_round_trip_preserves_edits_conflicts_and_deletions() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        let program = root.join("mock-proton-drive");
        fs::write(
            &program,
            include_str!("../tests/fixtures/fake-proton-drive"),
        )?;
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700))?;
        let local = root.join("local");
        let cloud = root.join("cloud");
        let trash = root.join("trash");
        let journal = root.join("state/journal.json");
        fs::create_dir_all(local.join("-local-child"))?;
        fs::create_dir_all(cloud.join("remote-child"))?;
        fs::write(local.join("-local-child/notes.txt"), "from local")?;
        fs::write(cloud.join("remote-child/list.txt"), "from cloud")?;
        fs::write(root.join("outside"), "do not upload")?;
        symlink(root.join("outside"), local.join("not-a-file"))?;

        let cli = Cli::for_test(program).with_local_trash_dir(trash.clone());
        let node = cli.info(ROOT)?;
        let binding = Binding::new(node.uid.clone(), node, local.clone(), "My files".into())?;

        let first = sync(&cli, &binding, &journal)?;
        assert_eq!(
            (first.uploads, first.downloads, first.conflicts),
            (1, 1, 1),
            "{:?}",
            first.details
        );
        assert!(!cloud.join("not-a-file").exists());
        assert_eq!(
            fs::read_to_string(cloud.join("-local-child/notes.txt"))?,
            "from local"
        );
        assert_eq!(
            fs::read_to_string(local.join("remote-child/list.txt"))?,
            "from cloud"
        );

        fs::write(local.join("-local-child/notes.txt"), "local edit")?;
        fs::write(cloud.join("remote-child/list.txt"), "cloud edit")?;
        let second = sync(&cli, &binding, &journal)?;
        assert_eq!(
            (second.uploads, second.downloads),
            (1, 1),
            "{:?}",
            second.details
        );
        assert_eq!(
            fs::read_to_string(cloud.join("-local-child/notes.txt"))?,
            "local edit"
        );
        assert_eq!(
            fs::read_to_string(local.join("remote-child/list.txt"))?,
            "cloud edit"
        );

        fs::write(local.join("-local-child/notes.txt"), "edit A")?;
        fs::write(cloud.join("-local-child/notes.txt"), "edit B")?;
        fs::remove_file(cloud.join("remote-child/list.txt"))?;
        let third = sync(&cli, &binding, &journal)?;
        assert_eq!(
            (
                third.uploads,
                third.downloads,
                third.conflicts,
                third.deletions
            ),
            (0, 0, 3, 0),
            "{:?}",
            third.details
        );
        assert_eq!(
            fs::read_to_string(local.join("-local-child/notes.txt"))?,
            "edit A"
        );
        assert_eq!(
            fs::read_to_string(cloud.join("-local-child/notes.txt"))?,
            "edit B"
        );
        // A failed UID lookup without Trash confirmation cannot remove the local file.
        assert_eq!(
            fs::read_to_string(local.join("remote-child/list.txt"))?,
            "cloud edit"
        );
        assert!(!trash.join("list.txt").exists());
        assert!(!cloud.join("remote-child/list.txt").exists());

        let mut wrong_account = binding;
        wrong_account.account = "other".into();
        assert!(
            sync(&cli, &wrong_account, &journal)
                .unwrap_err()
                .to_string()
                .contains("account changed")
        );
        Ok(())
    }

    struct Env {
        cli: Cli,
        binding: Binding,
        local: PathBuf,
        cloud: PathBuf,
        trash: PathBuf,
        journal: PathBuf,
    }

    fn env(root: &Path) -> Result<Env> {
        let program = root.join("mock-proton-drive");
        fs::write(
            &program,
            include_str!("../tests/fixtures/fake-proton-drive"),
        )?;
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700))?;
        let local = root.join("local");
        let cloud = root.join("cloud");
        let trash = root.join("trash");
        let journal = root.join("state/journal.json");
        fs::create_dir_all(&local)?;
        fs::create_dir_all(&cloud)?;
        fs::create_dir_all(&trash)?;
        let cli = Cli::for_test(program).with_local_trash_dir(trash.clone());
        let node = cli.info(ROOT)?;
        let binding = Binding::new(node.uid.clone(), node, local.clone(), "My files".into())?;
        Ok(Env {
            cli,
            binding,
            local,
            cloud,
            trash,
            journal,
        })
    }

    fn uid_of_remote(env: &Env, name: &str) -> Result<(String, String)> {
        for node in env.cli.list(ROOT)? {
            if node.name() == name {
                return Ok((node.uid.clone(), node.path()));
            }
        }
        bail!("Remote node {name} not found")
    }

    #[test]
    fn local_rename_replicates_to_remote() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let env = env(temp.path())?;
        fs::write(env.local.join("a.txt"), "hello")?;
        let first = sync(&env.cli, &env.binding, &env.journal)?;
        assert_eq!(first.uploads, 1, "{:?}", first.details);
        fs::rename(env.local.join("a.txt"), env.local.join("b.txt"))?;
        let second = sync(&env.cli, &env.binding, &env.journal)?;
        assert_eq!(second.moves, 1, "{:?}", second.details);
        assert_eq!(fs::read_to_string(env.cloud.join("b.txt"))?, "hello");
        assert!(!env.cloud.join("a.txt").exists());
        Ok(())
    }

    #[test]
    fn local_cross_dir_move_replicates_to_remote() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let env = env(temp.path())?;
        fs::create_dir_all(env.local.join("sub"))?;
        fs::write(env.local.join("a.txt"), "hi")?;
        assert_eq!(sync(&env.cli, &env.binding, &env.journal)?.uploads, 1);
        fs::rename(env.local.join("a.txt"), env.local.join("sub/a.txt"))?;
        let second = sync(&env.cli, &env.binding, &env.journal)?;
        assert_eq!(second.moves, 1, "{:?}", second.details);
        assert_eq!(fs::read_to_string(env.cloud.join("sub/a.txt"))?, "hi");
        assert!(!env.cloud.join("a.txt").exists());
        Ok(())
    }

    #[test]
    fn local_move_and_rename_into_new_directory() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let env = env(temp.path())?;
        fs::write(env.local.join("a.txt"), "original")?;
        sync(&env.cli, &env.binding, &env.journal)?;
        let (uid, _) = uid_of_remote(&env, "a.txt")?;
        fs::create_dir(env.local.join("sub"))?;
        fs::rename(env.local.join("a.txt"), env.local.join("sub/b.txt"))?;
        let report = sync(&env.cli, &env.binding, &env.journal)?;
        assert_eq!((report.moves, report.uploads, report.downloads), (1, 0, 0));
        assert_eq!(env.cli.info(&remote_path_for(&uid))?.name(), "b.txt");
        assert_eq!(fs::read_to_string(env.cloud.join("sub/b.txt"))?, "original");
        Ok(())
    }

    #[test]
    fn rename_with_local_edit_does_not_record_a_false_baseline() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let env = env(temp.path())?;
        fs::write(env.local.join("a.txt"), "original")?;
        sync(&env.cli, &env.binding, &env.journal)?;
        fs::rename(env.local.join("a.txt"), env.local.join("b.txt"))?;
        fs::write(env.local.join("b.txt"), "changed")?;
        let report = sync(&env.cli, &env.binding, &env.journal)?;
        assert_eq!(
            (
                report.moves,
                report.uploads,
                report.deletions,
                report.conflicts
            ),
            (0, 0, 0, 1)
        );
        assert_eq!(fs::read_to_string(env.cloud.join("a.txt"))?, "original");
        assert_eq!(fs::read_to_string(env.local.join("b.txt"))?, "changed");
        assert!(!env.cloud.join("b.txt").exists());
        Ok(())
    }

    #[test]
    fn a_symlink_at_a_tracked_path_is_not_a_deletion() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let env = env(temp.path())?;
        fs::write(env.local.join("a.txt"), "original")?;
        sync(&env.cli, &env.binding, &env.journal)?;
        fs::remove_file(env.local.join("a.txt"))?;
        symlink("missing", env.local.join("a.txt"))?;
        let report = sync(&env.cli, &env.binding, &env.journal)?;
        assert_eq!((report.deletions, report.conflicts), (0, 1));
        assert_eq!(fs::read_to_string(env.cloud.join("a.txt"))?, "original");
        Ok(())
    }

    #[test]
    fn local_delete_trashes_remote() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let env = env(temp.path())?;
        fs::write(env.local.join("a.txt"), "bye")?;
        assert_eq!(sync(&env.cli, &env.binding, &env.journal)?.uploads, 1);
        fs::remove_file(env.local.join("a.txt"))?;
        let second = sync(&env.cli, &env.binding, &env.journal)?;
        assert_eq!(second.deletions, 1, "{:?}", second.details);
        assert!(!env.cloud.join("a.txt").exists());
        Ok(())
    }

    #[test]
    fn copy_then_remove_is_not_assumed_to_be_a_deletion() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let env = env(temp.path())?;
        fs::write(env.local.join("a.txt"), "copy me")?;
        sync(&env.cli, &env.binding, &env.journal)?;
        fs::copy(env.local.join("a.txt"), env.local.join("b.txt"))?;
        fs::remove_file(env.local.join("a.txt"))?;
        let report = sync(&env.cli, &env.binding, &env.journal)?;
        assert_eq!(
            (
                report.moves,
                report.deletions,
                report.uploads,
                report.conflicts
            ),
            (0, 0, 0, 1)
        );
        assert_eq!(fs::read_to_string(env.cloud.join("a.txt"))?, "copy me");
        assert!(!env.cloud.join("b.txt").exists());
        Ok(())
    }

    #[test]
    fn remote_rename_replicates_to_local() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let env = env(temp.path())?;
        fs::write(env.local.join("a.txt"), "hello remote")?;
        assert_eq!(sync(&env.cli, &env.binding, &env.journal)?.uploads, 1);
        let (uid, _) = uid_of_remote(&env, "a.txt")?;
        env.cli.rename(&format!("{ROOT}/{uid}"), "b.txt", &uid)?;
        let second = sync(&env.cli, &env.binding, &env.journal)?;
        assert_eq!(second.moves, 1, "{:?}", second.details);
        assert_eq!(fs::read_to_string(env.local.join("b.txt"))?, "hello remote");
        assert!(!env.local.join("a.txt").exists());
        Ok(())
    }

    #[test]
    fn remote_cross_dir_move_replicates_to_local() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let env = env(temp.path())?;
        fs::write(env.local.join("a.txt"), "move me")?;
        assert_eq!(sync(&env.cli, &env.binding, &env.journal)?.uploads, 1);
        let (uid, _) = uid_of_remote(&env, "a.txt")?;
        let sub = env.cli.mkdir(ROOT, "sub")?;
        env.cli
            .move_node(&format!("{ROOT}/{uid}"), &sub.path(), &uid)?;
        let second = sync(&env.cli, &env.binding, &env.journal)?;
        assert_eq!(second.moves, 1, "{:?}", second.details);
        assert_eq!(fs::read_to_string(env.local.join("sub/a.txt"))?, "move me");
        assert!(!env.local.join("a.txt").exists());
        Ok(())
    }

    #[test]
    fn remote_delete_trashes_local() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let env = env(temp.path())?;
        fs::write(env.local.join("a.txt"), "delete me")?;
        assert_eq!(sync(&env.cli, &env.binding, &env.journal)?.uploads, 1);
        let (uid, _) = uid_of_remote(&env, "a.txt")?;
        env.cli.trash(&format!("{ROOT}/{uid}"), &uid)?;
        let second = sync(&env.cli, &env.binding, &env.journal)?;
        assert_eq!(second.deletions, 1, "{:?}", second.details);
        assert!(!env.local.join("a.txt").exists());
        assert_eq!(fs::read_to_string(env.trash.join("a.txt"))?, "delete me");
        Ok(())
    }

    #[test]
    fn occupied_remote_target_preserves_both() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let env = env(temp.path())?;
        fs::create_dir_all(env.local.join("sub"))?;
        fs::write(env.local.join("a.txt"), "aaa")?;
        fs::write(env.local.join("sub/b.txt"), "bbb")?;
        let first = sync(&env.cli, &env.binding, &env.journal)?;
        assert_eq!(first.uploads, 2, "{:?}", first.details);
        // Move local a.txt over sub/b.txt, which already exists on the remote
        // with a different UID and different bytes.
        fs::rename(env.local.join("a.txt"), env.local.join("sub/b.txt"))?;
        let second = sync(&env.cli, &env.binding, &env.journal)?;
        assert_eq!(second.moves, 0, "{:?}", second.details);
        assert!(env.cloud.join("a.txt").exists(), "baseline kept");
        assert_eq!(fs::read_to_string(env.cloud.join("sub/b.txt"))?, "bbb");
        assert_eq!(fs::read_to_string(env.local.join("sub/b.txt"))?, "aaa");
        Ok(())
    }

    #[test]
    fn local_delete_ignores_replaced_parent() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let env = env(temp.path())?;
        fs::create_dir_all(env.local.join("sub"))?;
        fs::write(env.local.join("sub/a.txt"), "mount data")?;
        assert_eq!(sync(&env.cli, &env.binding, &env.journal)?.uploads, 1);
        // Keep the old directory alive outside the sync tree so the filesystem
        // cannot reuse its inode for the replacement.
        fs::rename(env.local.join("sub"), temp.path().join("old-sub"))?;
        fs::create_dir_all(env.local.join("sub"))?;
        let second = sync(&env.cli, &env.binding, &env.journal)?;
        assert_eq!(second.deletions, 0, "{:?}", second.details);
        assert!(env.cloud.join("sub/a.txt").exists());
        Ok(())
    }

    #[test]
    fn no_mutations_on_failed_scan() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let env = env(temp.path())?;
        fs::write(env.local.join("a.txt"), "stable")?;
        assert_eq!(sync(&env.cli, &env.binding, &env.journal)?.uploads, 1);
        // Introduce an unsupported remote name and rename the file locally.
        fs::write(env.cloud.join("a\nb.txt"), "unsupported")?;
        fs::rename(env.local.join("a.txt"), env.local.join("b.txt"))?;
        let result = sync(&env.cli, &env.binding, &env.journal);
        assert!(
            result.is_err(),
            "unsupported remote name must fail the pass"
        );
        assert!(env.cloud.join("a.txt").exists(), "no move applied");
        assert!(!env.cloud.join("b.txt").exists());
        Ok(())
    }
}
