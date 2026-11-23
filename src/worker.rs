use crate::{
    drive::{Cli, Node, ROOT, STOP, Sharing, run_process},
    storage,
    sync::{self, Binding, Report},
};
use anyhow::{Context, Result, ensure};
use eframe::egui::{ColorImage, Context as UiContext};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufReader},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::Ordering,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const PERIOD: Duration = Duration::from_secs(60);
pub const PREVIEW_LIMIT: u64 = 32 * 1024 * 1024;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Preferences {
    pub theme: usize,
    pub details: bool,
    pub paused: bool,
    pub binding: Option<Binding>,
}

pub enum Request {
    Connect(bool),
    Logout,
    List(String),
    Configure {
        remote: String,
        label: String,
        local: PathBuf,
    },
    Pause(bool),
    Sync,
    ViewPrefs {
        theme: usize,
        details: bool,
    },
    Share(String, ShareAction),
    Thumbnail(Box<Node>),
    Save(Box<Node>, PathBuf),
    PickDirectory(u64),
}

pub enum ShareAction {
    Check,
    Invite { email: String, edit: bool },
    Link,
    Unlink,
}

pub enum Event {
    Preferences(Preferences),
    Account(Option<String>),
    Connecting(bool),
    Listing {
        path: String,
        nodes: Vec<Node>,
        cached: bool,
    },
    Listed {
        path: String,
        error: Option<String>,
    },
    Error(String),
    Notice(String),
    Sharing {
        path: String,
        result: Result<Sharing, String>,
    },
    Thumbnail {
        key: String,
        result: Result<ColorImage, String>,
    },
    Syncing,
    Synced(Result<Report, String>),
    PickedDirectory(u64, Option<String>),
}

pub struct Connection {
    pub send: Sender<Request>,
    pub receive: Receiver<Event>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        STOP.store(true, Ordering::Relaxed);
        // Wake the worker if it is waiting on the request queue.
        let _ = self.send.send(Request::Connect(false));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn connect(ctx: UiContext) -> Result<Connection> {
    let state = storage::xdg("XDG_STATE_HOME", ".local/state")?;
    let cache = storage::xdg("XDG_CACHE_HOME", ".cache")?;
    storage::private_dir(&state)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(state.join("instance.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock).context("Fuzzy Proton Drive is already running")?;
    let (send, requests) = mpsc::channel();
    let (events, receive) = mpsc::channel();
    let thread = thread::Builder::new()
        .name("proton-drive-worker".into())
        .spawn(move || {
            let _lock = lock;
            Worker {
                cli: Cli::new(),
                state,
                cache,
                events,
                ctx,
                account: None,
                prefs: Preferences::default(),
                deadline: Instant::now() + PERIOD,
            }
            .run(requests);
        })?;
    Ok(Connection {
        send,
        receive,
        thread: Some(thread),
    })
}

struct Worker {
    cli: Cli,
    state: PathBuf,
    cache: PathBuf,
    events: Sender<Event>,
    ctx: UiContext,
    account: Option<String>,
    prefs: Preferences,
    deadline: Instant,
}

impl Worker {
    fn event(&self, event: Event) {
        if self.events.send(event).is_ok() {
            self.ctx.request_repaint();
        }
    }
    fn run(mut self, requests: Receiver<Request>) {
        match storage::load::<Preferences>(&self.state.join("preferences.json")) {
            Ok(Some(prefs)) => self.prefs = prefs,
            Ok(None) => {}
            Err(err) => {
                self.event(Event::Error(format!(
                    "Settings could not be read; sync remains off: {err}"
                )));
            }
        }
        self.event(Event::Preferences(self.prefs.clone()));
        self.connect_account(false);
        while !STOP.load(Ordering::Relaxed) {
            let wait = self.deadline.saturating_duration_since(Instant::now());
            match requests.recv_timeout(wait) {
                _ if STOP.load(Ordering::Relaxed) => break,
                Ok(request) => {
                    if let Err(error) = self.handle(request) {
                        self.event(Event::Error(format!("{error:#}")));
                    }
                }
                Err(RecvTimeoutError::Timeout) => self.run_sync(),
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }
    fn connect_account(&mut self, login: bool) {
        self.event(Event::Connecting(true));
        let previous = self.account.take();
        if previous.is_some() {
            self.event(Event::Account(None));
        }
        let result = (|| {
            if login {
                self.cli.login()?;
            }
            self.cli.info(ROOT)
        })();
        match result {
            Ok(root) => {
                self.account = Some(root.uid);
                self.event(Event::Account(self.account.clone()));
                self.deadline = Instant::now() + Duration::from_secs(3);
            }
            Err(err) => self.event(Event::Error(format!("Drive unavailable: {err:#}"))),
        }
        self.event(Event::Connecting(false));
    }
    fn verify(&mut self) -> Result<String> {
        let account = self
            .account
            .clone()
            .context("Sign in to Proton Drive first")?;
        let actual = self.cli.info(ROOT)?.uid;
        if account != actual {
            self.account = None;
            self.event(Event::Account(None));
            anyhow::bail!("The CLI account changed. Reconnect before browsing or syncing.");
        }
        Ok(account)
    }
    fn save_prefs(&mut self, prefs: Preferences) -> Result<()> {
        storage::save(&self.state.join("preferences.json"), &prefs)?;
        self.prefs = prefs;
        self.event(Event::Preferences(self.prefs.clone()));
        Ok(())
    }
    fn handle(&mut self, req: Request) -> Result<()> {
        match req {
            Request::Connect(login) => self.connect_account(login),
            Request::Logout => {
                self.account = None;
                self.event(Event::Account(None));
                self.cli.logout()?;
                self.event(Event::Notice("Signed out".into()));
            }
            Request::List(path) => {
                let result = self.list(&path);
                self.event(Event::Listed {
                    path,
                    error: result.err().map(|e| format!("{e:#}")),
                });
            }
            Request::Configure {
                remote,
                label,
                local,
            } => {
                let account = self.verify()?;
                let node = self.cli.info(&remote)?;
                let binding = Binding::new(account, node, local, label)?;
                for private in [&self.state, &self.cache] {
                    let private = private.canonicalize().unwrap_or_else(|_| private.clone());
                    ensure!(
                        !private.starts_with(&binding.local)
                            && !binding.local.starts_with(&private),
                        "Choose a folder outside the application's cache and state directories"
                    );
                }
                let mut prefs = self.prefs.clone();
                prefs.binding = Some(binding);
                prefs.paused = false;
                self.save_prefs(prefs)?;
                self.deadline = Instant::now();
            }
            Request::Pause(paused) => {
                let mut prefs = self.prefs.clone();
                prefs.paused = paused;
                self.save_prefs(prefs)?;
                if !paused {
                    self.deadline = Instant::now();
                }
            }
            Request::Sync => self.run_sync(),
            Request::ViewPrefs { theme, details } => {
                let mut prefs = self.prefs.clone();
                prefs.theme = theme;
                prefs.details = details;
                self.save_prefs(prefs)?;
            }
            Request::Share(path, action) => {
                let result = (|| {
                    self.verify()?;
                    match action {
                        ShareAction::Check => self.cli.sharing(&path),
                        ShareAction::Invite { email, edit } => {
                            self.cli.invite(&path, email.trim(), edit)
                        }
                        ShareAction::Link => self.cli.link(&path),
                        ShareAction::Unlink => self.cli.unlink(&path),
                    }
                })();
                self.event(Event::Sharing {
                    path,
                    result: result.map_err(|e| format!("{e:#}")),
                });
            }
            Request::Thumbnail(node) => {
                let key = thumbnail_key(&node);
                let result = self.thumbnail(&node, &key);
                self.event(Event::Thumbnail {
                    key,
                    result: result.map_err(|e| format!("{e:#}")),
                });
            }
            Request::Save(node, dir) => {
                self.verify()?;
                let dir = dir
                    .canonicalize()
                    .context("Enter an existing destination folder")?;
                let stage = tempfile::tempdir_in(&dir)?;
                let from = self.cli.download(&node, stage.path())?;
                let to = dir.join(from.file_name().context("Missing file name")?);
                File::open(&from)?.sync_all()?;
                fs::hard_link(from, &to)
                    .context("Cannot save without overwriting an existing file")?;
                File::open(&dir)?.sync_all()?;
                self.event(Event::Notice(format!("Saved to {}", to.display())));
            }
            Request::PickDirectory(id) => {
                let events = self.events.clone();
                let ctx = self.ctx.clone();
                thread::Builder::new()
                    .name("kde-folder-dialog".into())
                    .spawn(move || {
                        use std::io::Read;
                        let mut picker = Command::new("kdialog");
                        picker.args(["--title", "Fuzzy Proton Drive", "--getexistingdirectory"]);
                        picker.arg(std::env::var_os("HOME").unwrap_or_else(|| "/".into()));
                        let path =
                            run_process(picker, Duration::from_secs(600))
                                .ok()
                                .and_then(|file| {
                                    let mut path = String::new();
                                    file.take(32 * 1024).read_to_string(&mut path).ok()?;
                                    let path = path.trim_end_matches('\n');
                                    path.starts_with('/').then(|| path.to_owned())
                                });
                        let _ = events.send(Event::PickedDirectory(id, path));
                        ctx.request_repaint();
                    })?;
            }
        }
        Ok(())
    }
    fn account_cache(&self, account: &str) -> PathBuf {
        self.cache
            .join(blake3::hash(account.as_bytes()).to_hex().as_str())
    }
    fn list(&mut self, path: &str) -> Result<()> {
        let account = self.account.clone().context("Sign in to browse Drive")?;
        let disk = self
            .account_cache(&account)
            .join("folders")
            .join(format!("{}.json", blake3::hash(path.as_bytes()).to_hex()));
        if let Ok(Some(nodes)) = storage::load(&disk) {
            self.event(Event::Listing {
                path: path.into(),
                nodes,
                cached: true,
            });
        }
        self.verify()?;
        let nodes = self.cli.list(path)?;
        if let Err(err) = storage::save(&disk, &nodes) {
            self.event(Event::Error(format!(
                "Could not save folder cache: {err:#}"
            )));
        }
        self.event(Event::Listing {
            path: path.into(),
            nodes,
            cached: false,
        });
        Ok(())
    }
    fn run_sync(&mut self) {
        self.deadline = Instant::now() + PERIOD;
        if let Some(binding) = &self.prefs.binding
            && self.account.as_ref() == Some(&binding.account)
            && !self.prefs.paused
        {
            self.event(Event::Syncing);
            let path = self
                .state
                .join("sync")
                .join(format!("{}.json", binding.key()));
            let result = sync::sync(&self.cli, binding, &path).map_err(|e| format!("{e:#}"));
            self.event(Event::Synced(result));
        }
        self.deadline = Instant::now() + PERIOD;
    }
    fn thumbnail(&mut self, node: &Node, key: &str) -> Result<ColorImage> {
        let account = self.account.clone().context("Sign in to preview media")?;
        let dir = self.account_cache(&account).join("previews");
        let file = dir.join(format!("{key}.png"));
        if let Ok(file) = File::open(&file)
            && let Ok(image) = decode(file)
        {
            return Ok(image);
        }
        self.verify()?;
        let file_info = self.cli.info(&node.path())?;
        ensure!(
            thumbnail_key(&file_info) == key,
            "The file changed; refresh the folder"
        );
        ensure!(
            file_info.visual() && file_info.size().is_some_and(|size| size <= PREVIEW_LIMIT),
            "Preview requires a known file size of 32 MiB or less"
        );
        storage::private_dir(&dir)?;
        let tmp = tempfile::tempdir_in(&dir)?;
        let input = self.cli.download(&file_info, tmp.path())?;
        ensure!(
            input.metadata()?.len() <= PREVIEW_LIMIT,
            "Preview exceeds the size limit"
        );
        ensure!(
            thumbnail_key(&self.cli.info(&node.path())?) == key,
            "The file changed during preview; refresh the folder"
        );
        let mut command = Command::new("ffmpeg");
        command.args([
            "-v", "error", "-nostdin", "-threads", "2", "-filter_threads", "1",
            "-protocol_whitelist", "file,pipe",
            "-format_whitelist", "image2,image2pipe,png_pipe,jpeg_pipe,webp_pipe,gif,bmp_pipe,tiff_pipe,mov,matroska,avi,mpeg,mpegts",
            "-i",
        ]).arg(input).args([
            "-map", "0:v:0", "-an", "-sn", "-frames:v", "1", "-vf",
            "scale=320:240:force_original_aspect_ratio=decrease:force_divisible_by=2,setsar=1",
            "-threads", "1", "-c:v", "png", "-f", "image2pipe", "pipe:1",
        ]);
        let mut output = run_process(command, Duration::from_secs(15))
            .context("Thumbnail generation failed (FFmpeg required)")?;
        let mut cached = tempfile::NamedTempFile::new_in(&dir)?;
        io::copy(&mut output, cached.as_file_mut())?;
        let image = decode(File::open(cached.path())?)?;
        cached.persist(file)?;
        trim_thumbnails(&dir);
        Ok(image)
    }
}

pub fn thumbnail_key(node: &Node) -> String {
    let version = node
        .active_revision
        .as_ref()
        .map(|r| r.uid.as_str())
        .unwrap_or("");
    let data = format!("preview-v1:{}:{version}", node.uid);
    blake3::hash(data.as_bytes()).to_hex().to_string()
}

fn decode(file: File) -> Result<ColorImage> {
    let mut reader = image::ImageReader::with_format(BufReader::new(file), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(320);
    limits.max_image_height = Some(240);
    limits.max_alloc = Some(4 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode()?.into_rgba8();
    Ok(ColorImage::from_rgba_unmultiplied(
        [image.width() as usize, image.height() as usize],
        image.as_raw(),
    ))
}

fn trim_thumbnails(dir: &Path) {
    if let Ok(files) = fs::read_dir(dir) {
        let mut files: Vec<_> = files
            .flatten()
            .filter_map(|file| {
                let path = file.path();
                if path.extension()?.to_str()? != "png" {
                    return None;
                }
                Some((file.metadata().ok()?.modified().ok()?, path))
            })
            .collect();
        if files.len() <= 512 {
            return;
        }
        files.sort_unstable();
        let remove = files.len() - 512;
        for (_, path) in files.into_iter().take(remove) {
            let _ = fs::remove_file(path);
        }
    }
}
