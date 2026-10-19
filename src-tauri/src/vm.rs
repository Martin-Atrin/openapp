//! App VM (contract §16, §17): one Linux VM per workspace, backed by microsandbox 0.7.8 (libkrun,
//! Hypervisor.framework). Grown from the 2026-10-15 VM spike (spec/IMPLEMENTATION-NOTES.md).
//!
//! - `/workspace` = the host workspace (virtio-fs bind, stat virtualization Off), `/opt/openapp` = a
//!   persistent ext4 volume (harness CLIs, their homes and logins). The rootfs is disposable.
//! - The VM has NO network device. Guest egress goes only through the kernel HTTP CONNECT proxy
//!   (`egress.rs`), reached through a guest-local port forwarded over vsock.
//! - Forwarders: the guest relay `vm/vsock-relay.py` (python3 from the guest image, written at boot) listens on the
//!   guest port and frames the stream over vsock in writes of at most 16 KiB; [`deframe`] undoes it on the host and
//!   acknowledges every 64 KiB. A raw `socat` route stalls after 256 KiB to a server that waits for the whole body
//!   (libkrun returns credit every 4 MiB, the guest kernel waits after its own 256 KiB buffer) and loses writes
//!   above ~32 KiB (multi-descriptor packets): the Pi "Connection error." of long chats. Details in the relay.
//!   - guest 127.0.0.1:7801 → vsock 7801 → host unix socket `run/<vm>/mcp.sock` → kernel MCP server (TCP)
//!   - guest 127.0.0.1:7802 → vsock 7802 → host unix socket `run/<vm>/proxy.sock` → kernel egress proxy
//!   - guest 127.0.0.1:7803 → vsock 7803 → host unix socket `run/<vm>/socks.sock` → kernel SOCKS5 egress
//!     (same policy; opt-in `OPENAPP_SOCKS_URL=socks5h://<actor>:openapp@127.0.0.1:7803`, proxychains4 for
//!     `"net": "socks"`; `ALL_PROXY` is never set: Python httpx refuses to start with a SOCKS ALL_PROXY)
//! - `run/<vm>/owner.pid` names the kernel process that owns the sockets; [`running_vms`] checks that the
//!   owner is alive and the socket accepts (a crashed or killed app leaves stale sockets, removed there).
//!   - host 127.0.0.1:<port> → per connection `socat STDIO TCP:127.0.0.1:<port>` exec'd in the guest
//!     (Codex login callback, port 1455)
//! - Processes in the guest (Codex app-server, module servers) are exec sessions with piped stdio
//!   (`GuestChild`); the kernel speaks the same JSONL / MCP over them as over a host child's pipes.
//!
//! Runtime: `msb` + `libkrunfw` from the app bundle (`bundle.rs`). Guest image: the archive at
//! `<vm home>/images/openapp-guest-<ver>.tar`, else downloaded once from [`GUEST_IMAGE_URL`] (resume,
//! retry, sha256-checked, progress on channel `kernel/vm-image` through [`on_image_progress`]).
//!
//! Env: `OPENAPP_VM_HOME` (state, default `~/.openapp/vm`; must be short: unix socket paths < 104
//! bytes), `OPENAPP_GUEST_IMAGE_TAR` (image archive for the first `Image::load`),
//! `OPENAPP_GUEST_IMAGE_URL` (download source instead of [`GUEST_IMAGE_URL`]), `OPENAPP_VM_CPUS`,
//! `OPENAPP_VM_MEMORY_MIB`. Debug builds only: `OPENAPP_MSB_DIR` (runtime: `bin/msb` +
//! `lib/libkrunfw.*`), then `<vm home>/runtime`, then the dev runtime `src-tauri/vm/runtime`
//! (`node scripts/fetch-runtime.ts`).

use crate::util::MutexExt;
use microsandbox::sandbox::StatVirtualization;
use microsandbox::{ExecControl, ExecEvent, Image, Sandbox, Volume};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Guest image version (keep in sync with `src-tauri/vm/guest-image/build.ts`).
pub const GUEST_IMAGE_VERSION: &str = "0.9.2";
pub const GUEST_WS: &str = "/workspace";
pub const GUEST_OPT: &str = "/opt/openapp";
pub const GUEST_HOME: &str = "/opt/openapp/home";
/// Guest-local port (and vsock port) of the kernel MCP server.
pub const MCP_PORT: u16 = 7801;
/// Guest-local port (and vsock port) of the kernel egress proxy.
pub const PROXY_PORT: u16 = 7802;
/// Guest-local port (and vsock port) of the kernel SOCKS5 egress (non-HTTP protocols).
pub const SOCKS_PORT: u16 = 7803;
/// Host unix sockets of a running VM, under `run/<vm>/`.
const SOCKETS: [&str; 3] = ["mcp.sock", "proxy.sock", "socks.sock"];
/// GitHub `owner/repo` that publishes the guest image release asset. Fill in before shipping a release.
macro_rules! guest_image_repo {
    () => {
        "Martin-Atrin/openapp"
    };
}
pub const GUEST_IMAGE_REPO: &str = guest_image_repo!();
/// Release asset of the guest image (zstd-compressed OCI archive). `OPENAPP_GUEST_IMAGE_URL` overrides it.
pub const GUEST_IMAGE_URL: &str =
    concat!("https://github.com/", guest_image_repo!(), "/releases/download/guest-0.9.2/openapp-guest-0.9.2.tar.zst");
/// sha256 + size of `openapp-guest-0.9.2.tar.zst` (`zstd -19 -T0` of the tar below).
pub const GUEST_IMAGE_ZST_SHA256: &str = "6704c019e93b9d8bcd501f183b625810ccd0f031f6826e1be0a4d58de7aef3d8";
pub const GUEST_IMAGE_ZST_SIZE: u64 = 368_824_968;
/// sha256 + size of `openapp-guest-0.9.2.tar` (output of `src-tauri/vm/guest-image/build.ts`).
pub const GUEST_IMAGE_TAR_SHA256: &str = "9effc5707cfc8d930547066f50bd6fe8482591fc9254e6836dc92ab1e47b0cc3";
pub const GUEST_IMAGE_TAR_SIZE: u64 = 369_736_192;
/// Free disk space kept beyond the download + archive (unpacked image layers, VM volume, headroom).
const IMAGE_DISK_BUDGET: u64 = 2 * 1024 * 1024 * 1024;
const OPT_VOLUME: &str = "openapp-opt";
/// §21 G3: ≥ 20 GiB, sparse (an older, smaller volume is grown in place before boot + `resize2fs` online).
const OPT_VOLUME_MIB: u32 = 20480;
/// macOS `sun_path` limit (104 bytes incl. NUL).
const SOCKET_PATH_MAX: usize = 103;

pub fn guest_image() -> String {
    format!("openapp-guest:{GUEST_IMAGE_VERSION}")
}

/// Resolved VM paths.
#[derive(Debug, Clone)]
pub struct VmEnv {
    pub home: PathBuf,
    pub msb: PathBuf,
    pub libkrunfw: PathBuf,
    pub image_tar: PathBuf,
}

#[cfg_attr(not(debug_assertions), allow(dead_code))]
fn libkrunfw_name() -> &'static str {
    if cfg!(target_os = "macos") { "libkrunfw.5.dylib" } else { "libkrunfw.so.5" }
}

pub fn vm_home() -> PathBuf {
    match std::env::var_os("OPENAPP_VM_HOME").filter(|p| !p.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".openapp").join("vm"),
    }
}

/// `(msb, libkrunfw)`. Release: the app bundle only (a missing piece is an error, never masked by
/// another path). Debug: `OPENAPP_MSB_DIR`, the app bundle, `<vm home>/runtime`, the dev runtime
/// `src-tauri/vm/runtime` (each a folder with `bin/msb` + `lib/libkrunfw.*`).
fn runtime_paths(home: &Path) -> Result<(PathBuf, PathBuf), String> {
    #[cfg(debug_assertions)]
    {
        let pick = |d: &Path| {
            let (m, l) = (d.join("bin").join("msb"), d.join("lib").join(libkrunfw_name()));
            (m.is_file() && l.is_file()).then(|| (m.canonicalize().unwrap_or(m), l.canonicalize().unwrap_or(l)))
        };
        if let Some(found) = std::env::var_os("OPENAPP_MSB_DIR").filter(|p| !p.is_empty()).and_then(|d| pick(Path::new(&d))) {
            return Ok(found);
        }
        if let (Some(m), Some(l)) = (crate::bundle::piece(crate::bundle::MSB), crate::bundle::piece(crate::bundle::LIBKRUNFW)) {
            return Ok((m, l));
        }
        [home.join("runtime"), PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/vm/runtime"))]
            .iter()
            .find_map(|d| pick(d))
            .ok_or_else(|| "App VM runtime not found: set OPENAPP_MSB_DIR to a folder with bin/msb and lib/libkrunfw (microsandbox 0.7.8)".to_string())
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = home;
        let m = crate::bundle::piece(crate::bundle::MSB).ok_or_else(|| crate::bundle::missing("the VM runtime (msb)", crate::bundle::MSB))?;
        let l = crate::bundle::piece(crate::bundle::LIBKRUNFW)
            .ok_or_else(|| crate::bundle::missing("the VM firmware (libkrunfw)", crate::bundle::LIBKRUNFW))?;
        Ok((m, l))
    }
}

/// Download source of the guest image: `OPENAPP_GUEST_IMAGE_URL`, else [`GUEST_IMAGE_URL`] once
/// [`GUEST_IMAGE_REPO`] is filled in.
pub fn image_url() -> Option<String> {
    if let Some(u) = std::env::var("OPENAPP_GUEST_IMAGE_URL").ok().filter(|u| !u.trim().is_empty()) {
        return Some(u.trim().to_string());
    }
    (!GUEST_IMAGE_REPO.contains('<')).then(|| GUEST_IMAGE_URL.to_string())
}

/// The guest image is loaded, its archive is on disk, or it can be downloaded.
pub fn image_available(env: &VmEnv) -> bool {
    env.image_tar.is_file() || loaded_marker(&env.home).is_file() || image_url().is_some()
}

/// Resolve and check the VM runtime (msb + libkrunfw + guest image source). Cheap; used by detection.
pub fn env_check() -> Result<VmEnv, String> {
    let home = vm_home();
    let (msb, libkrunfw) = runtime_paths(&home)?;
    let image_tar = std::env::var_os("OPENAPP_GUEST_IMAGE_TAR")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("images").join(format!("openapp-guest-{GUEST_IMAGE_VERSION}.tar")));
    // run/<name>/proxy.sock and the runtime's own sockets live under home
    if home.to_string_lossy().len() + 48 > SOCKET_PATH_MAX {
        return Err(format!("OPENAPP_VM_HOME is too long for unix sockets ({}); use a shorter path", home.display()));
    }
    let env = VmEnv { home, msb, libkrunfw, image_tar };
    if !image_available(&env) {
        return Err(if cfg!(debug_assertions) {
            format!(
                "App VM guest image not found ({}): run `node src-tauri/vm/guest-image/build.ts` or set OPENAPP_GUEST_IMAGE_URL",
                env.image_tar.display()
            )
        } else {
            "The OpenApp VM image cannot be downloaded: this build has no download address (GUEST_IMAGE_REPO)".to_string()
        });
    }
    Ok(env)
}

fn loaded_marker(home: &Path) -> PathBuf {
    home.join("images").join(format!(".loaded-{GUEST_IMAGE_VERSION}"))
}

static SDK: OnceLock<Result<VmEnv, String>> = OnceLock::new();

/// Point the microsandbox SDK at our runtime and state home (process-wide, once).
fn init_sdk() -> Result<VmEnv, String> {
    SDK.get_or_init(|| {
        let env = env_check()?;
        std::fs::create_dir_all(&env.home).map_err(|e| format!("create {}: {e}", env.home.display()))?;
        microsandbox::config::set_sdk_msb_path(&env.msb);
        microsandbox::set_libkrunfw_path(&env.libkrunfw);
        let backend = microsandbox::backend::LocalBackend::builder()
            .home(&env.home)
            .config_path(env.home.join("config.json"))
            .build_lazy()
            .map_err(|e| format!("microsandbox backend: {e}"))?;
        microsandbox::backend::set_default_backend(backend);
        Ok(env)
    })
    .clone()
}

/// Load the guest image into the msb cache if missing (downloading its archive first when needed).
/// Returns ms spent getting + loading (0 = cached).
async fn ensure_image(env: &VmEnv) -> Result<u64, String> {
    let r = guest_image();
    if Image::get(&r).await.is_ok() {
        mark_loaded(env, &r);
        return Ok(0);
    }
    static PREPARE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _g = PREPARE.lock().await;
    if Image::get(&r).await.is_ok() {
        return Ok(0);
    }
    let t = Instant::now();
    if !env.image_tar.is_file() {
        let url = image_url().ok_or_else(|| format!("guest image {r} is not loaded, {} is missing and there is no download address", env.image_tar.display()))?;
        if let Err(e) = image_fetch::fetch(&url, &env.image_tar).await {
            image_fetch::report(json!({ "phase": "error", "error": e, "note": format!("Could not download the OpenApp VM: {e}") }));
            return Err(e);
        }
    }
    image_fetch::report(json!({ "phase": "loading", "note": "Preparing the OpenApp VM" }));
    if let Err(e) = Image::load(&env.image_tar, vec![r.clone()]).await {
        let e = format!("load {}: {e}", env.image_tar.display());
        image_fetch::report(json!({ "phase": "error", "error": e, "note": format!("Could not prepare the OpenApp VM: {e}") }));
        return Err(e);
    }
    mark_loaded(env, &r);
    let ms = t.elapsed().as_millis() as u64;
    image_fetch::report(json!({ "phase": "ready", "ms": ms, "note": "OpenApp VM ready" }));
    Ok(ms)
}

/// The `.loaded-<ver>` marker: this version is in the msb cache (an image switch loads the new archive once;
/// the `/opt/openapp` volume with the installed harness CLIs is independent of the image and is kept).
fn mark_loaded(env: &VmEnv, r: &str) {
    if !loaded_marker(&env.home).is_file() {
        let _ = std::fs::create_dir_all(env.home.join("images"));
        let _ = std::fs::write(loaded_marker(&env.home), r);
    }
}

/// After a boot on the current image: drop older guest images (msb cache, `.loaded-*` markers, archives in
/// `images/`). Best effort; an image still referenced elsewhere stays (non-forced remove).
async fn prune_old_images(env: &VmEnv) {
    let cur = guest_image();
    if let Ok(list) = Image::list().await {
        for h in list {
            let r = h.reference();
            if r.contains("openapp-guest:") && !r.ends_with(&cur) {
                match Image::remove(r, false).await {
                    Ok(()) => log::info!("app VM: removed old guest image {r}"),
                    Err(e) => log::info!("app VM: old guest image {r} kept: {e}"),
                }
            }
        }
    }
    let keep = [format!(".loaded-{GUEST_IMAGE_VERSION}"), format!("openapp-guest-{GUEST_IMAGE_VERSION}.tar")];
    let Ok(rd) = std::fs::read_dir(env.home.join("images")) else { return };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        let old = (n.starts_with(".loaded-") || (n.starts_with("openapp-guest-") && (n.ends_with(".tar") || n.ends_with(".tar.zst"))))
            && !keep.contains(&n);
        if old && e.path() != env.image_tar {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

pub use image_fetch::{image_progress, on_image_progress};

/// First-run download of the guest image archive: disk check, HTTP range resume, retry with backoff,
/// sha256 verification of the download and of the decompressed archive.
mod image_fetch {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};

    type Hook = Arc<dyn Fn(&Value) + Send + Sync>;
    static HOOKS: Mutex<Vec<Hook>> = Mutex::new(Vec::new());
    static LAST: Mutex<Option<Value>> = Mutex::new(None);
    const ATTEMPTS: u32 = 8;
    const ROUNDS: u32 = 2;
    const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

    /// Call `f` with every progress update `{ phase, note, done?, total?, error? }` (phases: checking,
    /// downloading, verifying, decompressing, loading, ready, error).
    pub fn on_image_progress(f: Hook) {
        HOOKS.locked().push(f);
    }

    /// The last progress update (null before the first download / load in this process).
    pub fn image_progress() -> Value {
        LAST.locked().clone().unwrap_or(Value::Null)
    }

    pub fn report(v: Value) {
        *LAST.locked() = Some(v.clone());
        let hooks = HOOKS.locked().clone();
        for h in hooks {
            h(&v);
        }
    }

    fn mb(b: u64) -> u64 {
        b.div_ceil(1_000_000)
    }

    /// Free bytes on the volume of `dir`.
    fn free_bytes(dir: &Path) -> Option<u64> {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: c is NUL-terminated, st is a valid out pointer
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
            return None;
        }
        #[allow(clippy::unnecessary_cast)] // the field types differ between platforms
        Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
    }

    fn part_len(p: &Path) -> u64 {
        std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
    }

    /// Download `url` (zstd archive or plain tar), verify it and leave the tar at `tar`.
    pub async fn fetch(url: &str, tar: &Path) -> Result<(), String> {
        let dir = tar.parent().ok_or("bad image path")?.to_path_buf();
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let name = tar.file_name().and_then(|n| n.to_str()).unwrap_or("openapp-guest.tar");
        let part = dir.join(format!("{name}.download"));
        let tar_part = dir.join(format!("{name}.partial"));
        report(json!({ "phase": "checking", "note": "Checking disk space for the OpenApp VM" }));
        let need = GUEST_IMAGE_ZST_SIZE.saturating_sub(part_len(&part)) + GUEST_IMAGE_TAR_SIZE + IMAGE_DISK_BUDGET;
        if let Some(free) = free_bytes(&dir)
            && free < need
        {
            return Err(format!(
                "not enough disk space for the OpenApp VM: {:.1} GB free, {:.1} GB needed",
                free as f64 / 1e9,
                need as f64 / 1e9
            ));
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(60))
            .user_agent(concat!("OpenApp/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        let mut last_err = String::new();
        for round in 0..ROUNDS {
            download(&client, url, &part).await?;
            report(json!({ "phase": "verifying", "note": "Checking the OpenApp VM download" }));
            let p = part.clone();
            let t = tar_part.clone();
            let res = tokio::task::spawn_blocking(move || verify_and_unpack(&p, &t)).await.map_err(|e| e.to_string())?;
            match res {
                Ok(()) => {
                    std::fs::rename(&tar_part, tar).map_err(|e| format!("rename {}: {e}", tar.display()))?;
                    let _ = std::fs::remove_file(&part);
                    log::info!("guest image downloaded and verified: {}", tar.display());
                    return Ok(());
                }
                Err(e) => {
                    // a corrupt download is never resumed: start over once
                    log::warn!("guest image check failed (round {}): {e}", round + 1);
                    let _ = std::fs::remove_file(&part);
                    let _ = std::fs::remove_file(&tar_part);
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    /// Fill `part` from `url` (resuming what is there), with retries and backoff.
    async fn download(client: &reqwest::Client, url: &str, part: &Path) -> Result<(), String> {
        let mut err = String::new();
        for attempt in 1..=ATTEMPTS {
            match download_once(client, url, part).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    err = e;
                    if attempt == ATTEMPTS {
                        break;
                    }
                    let wait = Duration::from_secs((1u64 << (attempt - 1)).min(30));
                    log::warn!("guest image download attempt {attempt} failed: {err}; retrying in {} s", wait.as_secs());
                    let have = part_len(part);
                    report(json!({
                        "phase": "downloading", "done": have, "total": GUEST_IMAGE_ZST_SIZE, "attempt": attempt, "error": err,
                        "note": format!("Downloading the OpenApp VM — {} / {} MB (retrying)", mb(have), mb(GUEST_IMAGE_ZST_SIZE)),
                    }));
                    tokio::time::sleep(wait).await;
                }
            }
        }
        Err(format!("download failed after {ATTEMPTS} attempts: {err}"))
    }

    async fn download_once(client: &reqwest::Client, url: &str, part: &Path) -> Result<(), String> {
        let mut have = part_len(part);
        let mut req = client.get(url);
        if have > 0 {
            req = req.header(reqwest::header::RANGE, format!("bytes={have}-"));
        }
        let mut resp = req.send().await.map_err(|e| format!("request: {e}"))?;
        let status = resp.status().as_u16();
        let total = match status {
            206 => resp
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.rsplit('/').next())
                .and_then(|t| t.parse::<u64>().ok())
                .or_else(|| resp.content_length().map(|l| l + have)),
            200 => {
                have = 0;
                resp.content_length()
            }
            // the part already holds the whole file
            416 if have > 0 => return Ok(()),
            s => return Err(format!("HTTP {s} from {url}")),
        };
        let mut f = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(have > 0)
            .truncate(have == 0)
            .open(part)
            .await
            .map_err(|e| format!("open {}: {e}", part.display()))?;
        let shown_total = total.unwrap_or(GUEST_IMAGE_ZST_SIZE);
        let mut done = have;
        let mut last = Instant::now().checked_sub(Duration::from_secs(1)).unwrap_or_else(Instant::now);
        loop {
            let chunk = match resp.chunk().await {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    let _ = f.flush().await;
                    return Err(format!("download interrupted at {} MB: {e}", mb(done)));
                }
            };
            f.write_all(&chunk).await.map_err(|e| format!("write {}: {e}", part.display()))?;
            done += chunk.len() as u64;
            if last.elapsed() >= Duration::from_millis(250) || Some(done) == total {
                last = Instant::now();
                report(json!({
                    "phase": "downloading", "done": done, "total": shown_total,
                    "note": format!("Downloading the OpenApp VM — {} / {} MB", mb(done), mb(shown_total)),
                }));
            }
        }
        f.flush().await.map_err(|e| e.to_string())?;
        f.sync_all().await.map_err(|e| e.to_string())?;
        if let Some(t) = total
            && done < t
        {
            return Err(format!("download ended early ({} of {} MB)", mb(done), mb(t)));
        }
        Ok(())
    }

    fn sha256_file(p: &Path) -> Result<String, String> {
        let mut f = std::fs::File::open(p).map_err(|e| format!("open {}: {e}", p.display()))?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        Ok(format!("{:x}", h.finalize()))
    }

    /// Writer that hashes and counts what goes through it.
    struct HashWrite<W: Write> {
        inner: W,
        h: Sha256,
        n: u64,
        last: Instant,
    }

    impl<W: Write> Write for HashWrite<W> {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            let n = self.inner.write(b)?;
            self.h.update(&b[..n]);
            self.n += n as u64;
            if self.last.elapsed() >= Duration::from_millis(250) {
                self.last = Instant::now();
                report(json!({
                    "phase": "decompressing", "done": self.n, "total": GUEST_IMAGE_TAR_SIZE,
                    "note": format!("Unpacking the OpenApp VM — {} / {} MB", mb(self.n), mb(GUEST_IMAGE_TAR_SIZE)),
                }));
            }
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }

    /// Check the download (zstd: its sha256, then the tar's while unpacking; plain tar: the tar's) and
    /// write the tar to `out`.
    fn verify_and_unpack(part: &Path, out: &Path) -> Result<(), String> {
        let mut magic = [0u8; 4];
        let is_zstd = std::fs::File::open(part).and_then(|mut f| f.read_exact(&mut magic)).is_ok() && magic == ZSTD_MAGIC;
        if !is_zstd {
            let got = sha256_file(part)?;
            if got != GUEST_IMAGE_TAR_SHA256 {
                return Err(format!("the downloaded VM image is damaged (sha256 {got}, expected {GUEST_IMAGE_TAR_SHA256})"));
            }
            return std::fs::copy(part, out).map(|_| ()).map_err(|e| format!("copy {}: {e}", out.display()));
        }
        let got = sha256_file(part)?;
        if got != GUEST_IMAGE_ZST_SHA256 {
            return Err(format!("the downloaded VM image is damaged (sha256 {got}, expected {GUEST_IMAGE_ZST_SHA256})"));
        }
        let src = std::fs::File::open(part).map_err(|e| format!("open {}: {e}", part.display()))?;
        let dst = std::fs::File::create(out).map_err(|e| format!("create {}: {e}", out.display()))?;
        let mut w = HashWrite { inner: std::io::BufWriter::new(dst), h: Sha256::new(), n: 0, last: Instant::now() };
        zstd::stream::copy_decode(std::io::BufReader::new(src), &mut w).map_err(|e| format!("unpack the VM image: {e}"))?;
        w.flush().map_err(|e| e.to_string())?;
        let HashWrite { inner, h, n, .. } = w;
        inner.into_inner().map_err(|e| e.to_string())?.sync_all().map_err(|e| e.to_string())?;
        let got = format!("{:x}", h.finalize());
        if got != GUEST_IMAGE_TAR_SHA256 || n != GUEST_IMAGE_TAR_SIZE {
            return Err(format!("the unpacked VM image is damaged (sha256 {got}, {n} bytes)"));
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn url_and_constants() {
            assert!(GUEST_IMAGE_URL.contains(GUEST_IMAGE_VERSION) && GUEST_IMAGE_URL.ends_with(".tar.zst"));
            assert!(GUEST_IMAGE_URL.contains(GUEST_IMAGE_REPO));
            assert_eq!(GUEST_IMAGE_ZST_SHA256.len(), 64);
            assert_eq!(GUEST_IMAGE_TAR_SHA256.len(), 64);
            assert!(free_bytes(Path::new("/")).is_some_and(|b| b > 0));
            assert_eq!(mb(368_645_878), 369);
        }

        /// Serve a small file over a local HTTP server with range support, drop the connection once
        /// mid-way, and check download + resume produce the same bytes.
        #[tokio::test]
        async fn download_resumes_after_a_drop() {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let body: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = l.local_addr().unwrap().port();
            let served = body.clone();
            tokio::spawn(async move {
                let mut n = 0;
                while let Ok((mut s, _)) = l.accept().await {
                    n += 1;
                    let mut req = vec![0u8; 4096];
                    let k = s.read(&mut req).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&req[..k]).to_string();
                    let start: usize = req
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("range: bytes=").map(|r| r.trim_end_matches('-').trim().to_string()))
                        .and_then(|r| r.parse().ok())
                        .unwrap_or(0);
                    let rest = &served[start..];
                    let head = if start > 0 {
                        format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n", rest.len(), start, served.len() - 1, served.len())
                    } else {
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", rest.len())
                    };
                    let _ = s.write_all(head.as_bytes()).await;
                    // the first connection breaks after 100 kB
                    let upto = if n == 1 { 100_000 } else { rest.len() };
                    let _ = s.write_all(&rest[..upto]).await;
                    let _ = s.shutdown().await;
                }
            });
            let dir = std::env::temp_dir().join(format!("oa-img-test-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let part = dir.join("x.download");
            let client = reqwest::Client::new();
            download(&client, &format!("http://127.0.0.1:{port}/img"), &part).await.unwrap();
            assert_eq!(std::fs::read(&part).unwrap(), body);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

/// Create the persistent volume, or grow an existing smaller one (sparse) to `mib`. Returns true when
/// the disk file was grown (then the filesystem is grown online in the guest after boot).
async fn ensure_volume(home: &Path, name: &str, mib: u32) -> Result<bool, String> {
    if Volume::get(name).await.is_ok() {
        // microsandbox 0.7.8 keeps a disk volume as <home>/volumes/<name>/disk.raw (raw ext4, sparse)
        let disk = home.join("volumes").join(name).join("disk.raw");
        let want = u64::from(mib) * 1024 * 1024;
        if let Ok(m) = std::fs::metadata(&disk)
            && m.is_file()
            && m.len() < want
        {
            let f = std::fs::OpenOptions::new().write(true).open(&disk).map_err(|e| format!("open {}: {e}", disk.display()))?;
            f.set_len(want).map_err(|e| format!("grow {}: {e}", disk.display()))?;
            log::info!("app VM volume {name} grown from {} to {} MiB", m.len() / 1024 / 1024, mib);
            return Ok(true);
        }
        return Ok(false);
    }
    Volume::builder(name).disk().size(mib).create().await.map(|_| false).map_err(|e| format!("create volume {name}: {e}"))
}

/// §21 G5: extra bind mounts `(guest path, host dir)` for the workspace VM — the user's granted folders
/// (`primitives/files.rs`, security-scoped bookmarks resolved to their current paths).
pub fn extra_mounts(host_ws: &Path) -> Vec<(String, PathBuf)> {
    crate::primitives::files::vm_mounts(host_ws)
}

/// Short, stable sandbox name for a workspace.
pub fn vm_name(ws: &Path) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in ws.to_string_lossy().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("oa-{:08x}", (h >> 32) as u32 ^ h as u32)
}

/// `HTTP(S)_PROXY` value for a guest process; the user part names the actor in the egress log.
pub fn proxy_url(actor: &str) -> String {
    format!("http://{actor}@127.0.0.1:{PROXY_PORT}")
}

/// `OPENAPP_SOCKS_URL` of a guest process: SOCKS5 with host-side name resolution; the user name is the actor.
pub fn socks_url(actor: &str) -> String {
    format!("socks5h://{actor}:openapp@127.0.0.1:{SOCKS_PORT}")
}

/// Proxy env for a guest process (git, npm, curl, pip, reqwest, Node fetch with NODE_USE_ENV_PROXY). SOCKS is opt-in:
/// `OPENAPP_SOCKS_URL` for libraries given a proxy option (IMAP, MTProto, MQTT, raw TCP); no `ALL_PROXY` (it would
/// switch every httpx / curl client to SOCKS and break httpx without `httpx[socks]`).
pub fn proxy_env(actor: &str) -> Vec<(String, String)> {
    let p = proxy_url(actor);
    let socks = socks_url(actor);
    let no = "localhost,127.0.0.1,::1".to_string();
    vec![
        ("HTTP_PROXY".into(), p.clone()),
        ("HTTPS_PROXY".into(), p.clone()),
        ("http_proxy".into(), p.clone()),
        ("https_proxy".into(), p),
        ("OPENAPP_SOCKS_URL".into(), socks),
        ("NO_PROXY".into(), no.clone()),
        ("no_proxy".into(), no),
        ("NODE_USE_ENV_PROXY".into(), "1".into()),
    ]
}

/// `"net": "socks"`: run `cmd` under proxychains4 (every TCP connection through the kernel SOCKS5 egress, names
/// resolved on the host) with a per-actor copy of the image's `/etc/proxychains4.conf`. Guest-local addresses
/// (127.0.0.0/8, e.g. the kernel at 127.0.0.1:7801) stay direct. Does not reach into `qemu-x86_64` programs.
pub fn socks_wrap(actor: &str, cmd: &[String]) -> Vec<String> {
    let actor: String = actor.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')).collect();
    let script = format!(
        "c=/tmp/openapp-proxychains-{actor}.conf; sed 's/^socks5 .*/socks5 127.0.0.1 {SOCKS_PORT} {actor} openapp/' /etc/proxychains4.conf > \"$c\" && exec proxychains4 -q -f \"$c\" \"$@\""
    );
    let mut v = vec!["sh".to_string(), "-c".into(), script, "openapp-socks".into()];
    v.extend(cmd.iter().cloned());
    v
}

/// `net` key of a service / server spec: `"socks"` wraps the command with [`socks_wrap`].
pub fn wants_socks(spec: &Value) -> bool {
    spec.get("net").and_then(Value::as_str) == Some("socks")
}

/// VMs under `<home>/run/` whose sockets belong to a live kernel process. Stale sockets (owner gone, or nobody
/// accepts) are removed. Used before booting a second kernel on the same VM home (two VMs must never mount one
/// `/opt/openapp` volume).
pub fn running_vms(home: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(home.join("run")) else { return Vec::new() };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let dir = e.path();
        if !dir.join("proxy.sock").exists() {
            continue;
        }
        if live(&dir) {
            out.push(e.file_name().to_string_lossy().to_string());
        } else {
            log::info!("app VM: removing stale sockets in {}", dir.display());
            remove_sockets(&dir);
        }
    }
    out
}

/// The sockets in `run_dir` belong to a live process (its `owner.pid` exists, when written) and accept.
fn live(run_dir: &Path) -> bool {
    let owner_alive = std::fs::read_to_string(run_dir.join("owner.pid"))
        .ok()
        .and_then(|p| p.trim().parse::<i32>().ok())
        // SAFETY: kill(pid, 0) only checks that the process exists
        .map(|pid| pid > 0 && (unsafe { libc::kill(pid, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)));
    owner_alive != Some(false) && std::os::unix::net::UnixStream::connect(run_dir.join("proxy.sock")).is_ok()
}

fn remove_sockets(run_dir: &Path) {
    for s in SOCKETS {
        let _ = std::fs::remove_file(run_dir.join(s));
    }
    let _ = std::fs::remove_file(run_dir.join("owner.pid"));
}

/// What happens to a connection that arrives on a host unix socket of a vsock route (already deframed).
pub type StreamHandler = Arc<dyn Fn(DuplexStream) + Send + Sync>;

/// The guest side of every vsock route (see the module docs).
const VSOCK_RELAY_PY: &str = include_str!("../vm/vsock-relay.py");
const VSOCK_RELAY_GUEST: &str = "/run/openapp/vsock-relay.py";
/// Frames of the vsock route protocol: `[type u8][len u32 BE][payload]`.
const FRAME_DATA: u8 = 0;
const FRAME_END: u8 = 1;
const FRAME_ACK: u8 = 2;
/// Bytes from the guest between two acks (the route's credit is 256 KiB).
const ACK_EVERY: usize = 64 * 1024;
/// Largest frame accepted from the guest (the relay sends at most 64 KiB).
const FRAME_LIMIT: usize = 1 << 20;

fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(5 + payload.len());
    f.push(kind);
    f.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    f.extend_from_slice(payload);
    f
}

/// One vsock route connection as a plain byte stream: data frames from the guest relay come out of the returned
/// stream, what is written into it goes back as data frames, and every [`ACK_EVERY`] bytes from the guest are
/// acknowledged. The host keeps its side open until the guest closes (a host close can overtake queued data).
pub fn deframe(us: tokio::net::UnixStream) -> DuplexStream {
    let (app, inner) = tokio::io::duplex(256 * 1024);
    tokio::spawn(relay_frames(us, inner));
    app
}

async fn relay_frames(us: tokio::net::UnixStream, inner: DuplexStream) {
    let (mut ur, mut uw) = us.into_split();
    let (mut ir, mut iw) = tokio::io::split(inner);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let writer = tokio::spawn(async move {
        while let Some(f) = rx.recv().await {
            if uw.write_all(&f).await.is_err() {
                break;
            }
        }
    });
    let down_tx = tx.clone();
    let down = tokio::spawn(async move {
        let mut buf = vec![0u8; ACK_EVERY];
        loop {
            let n = ir.read(&mut buf).await.unwrap_or(0);
            let f = if n == 0 { frame(FRAME_END, &[]) } else { frame(FRAME_DATA, &buf[..n]) };
            if down_tx.send(f).await.is_err() || n == 0 {
                break;
            }
        }
    });
    let mut head = [0u8; 5];
    let mut since_ack = 0usize;
    let mut open = true;
    while ur.read_exact(&mut head).await.is_ok() {
        let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
        match head[0] {
            FRAME_DATA if len <= FRAME_LIMIT => {
                let mut p = vec![0u8; len];
                if ur.read_exact(&mut p).await.is_err() {
                    break;
                }
                if open && iw.write_all(&p).await.is_err() {
                    open = false;
                }
                since_ack += len;
                if since_ack >= ACK_EVERY {
                    since_ack = 0;
                    // a full queue already carries data frames, which refresh the credit too
                    let _ = tx.try_send(frame(FRAME_ACK, &[]));
                }
            }
            FRAME_END => {
                if open {
                    let _ = iw.shutdown().await;
                    open = false;
                }
            }
            _ => break,
        }
    }
    // the guest closed (or broke the protocol): the app side sees the end, the host side closes last
    let _ = iw.shutdown().await;
    down.abort();
    drop(tx);
    let _ = writer.await;
}

pub struct AppVm {
    pub sandbox: Sandbox,
    pub name: String,
    pub host_ws: PathBuf,
    run_dir: PathBuf,
    alive: Arc<AtomicBool>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    /// `command -v` of common tools in the guest (for describe).
    pub guest_tools: Mutex<Value>,
    /// Boot timings (image load, create, total).
    pub timings: Mutex<Value>,
}

impl AppVm {
    /// Boot the workspace VM: image, volume, vsock routes, forwarders. `mcp_port` = kernel MCP
    /// server port on host 127.0.0.1; `proxy` handles each egress connection from the guest.
    pub async fn start(host_ws: &Path, mcp_port: u16, proxy: StreamHandler, socks: StreamHandler) -> Result<Arc<AppVm>, String> {
        let t0 = Instant::now();
        let env = init_sdk()?;
        let image_ms = ensure_image(&env).await?;
        let grown = ensure_volume(&env.home, OPT_VOLUME, OPT_VOLUME_MIB).await?;
        let name = vm_name(host_ws);
        let run_dir = env.home.join("run").join(&name);
        std::fs::create_dir_all(&run_dir).map_err(|e| format!("create {}: {e}", run_dir.display()))?;
        // two kernels must never run one workspace VM (one /opt/openapp volume); stale sockets are replaced
        let owner = std::fs::read_to_string(run_dir.join("owner.pid")).ok().and_then(|p| p.trim().parse::<u32>().ok());
        if owner != Some(std::process::id()) && live(&run_dir) {
            return Err(format!("another OpenApp process (pid {}) runs this workspace's VM; quit it first", owner.map(|p| p.to_string()).unwrap_or_else(|| "?".into())));
        }
        let mcp_sock = run_dir.join("mcp.sock");
        let proxy_sock = run_dir.join("proxy.sock");
        let socks_sock = run_dir.join("socks.sock");
        let alive = Arc::new(AtomicBool::new(true));
        let mut tasks = Vec::new();
        // the host sockets must exist before the VM boots; owner.pid lets other tools tell live from stale
        let mcp_l = bind_unix(&mcp_sock)?;
        let proxy_l = bind_unix(&proxy_sock)?;
        let socks_l = bind_unix(&socks_sock)?;
        let _ = std::fs::write(run_dir.join("owner.pid"), std::process::id().to_string());
        tasks.push(tokio::spawn(accept_loop(mcp_l, Arc::new(move |s: DuplexStream| {
            tokio::spawn(async move {
                let mut s = s;
                match tokio::net::TcpStream::connect(("127.0.0.1", mcp_port)).await {
                    Ok(mut t) => {
                        let _ = tokio::io::copy_bidirectional(&mut s, &mut t).await;
                    }
                    Err(e) => log::warn!("vm: MCP forward connect failed: {e}"),
                }
            });
        }))));
        tasks.push(tokio::spawn(accept_loop(proxy_l, proxy)));
        tasks.push(tokio::spawn(accept_loop(socks_l, socks)));

        let cpus: u8 = std::env::var("OPENAPP_VM_CPUS").ok().and_then(|v| v.parse().ok()).unwrap_or(2);
        let mem: u32 = std::env::var("OPENAPP_VM_MEMORY_MIB").ok().and_then(|v| v.parse().ok()).unwrap_or(4096);
        let ws = host_ws.to_path_buf();
        let mut b = Sandbox::builder(name.clone())
            .image(guest_image().as_str())
            .cpus(cpus)
            .memory(mem)
            .volume(GUEST_WS, move |v| v.bind(ws).stat_virtualization(StatVirtualization::Off))
            .volume(GUEST_OPT, |v| v.named(OPT_VOLUME))
            .disable_network()
            .env("HOME", GUEST_HOME)
            .env("LANG", "C.UTF-8")
            .workdir(GUEST_WS)
            .vsock(&mcp_sock, MCP_PORT as u32)
            .vsock(&proxy_sock, PROXY_PORT as u32)
            .vsock(&socks_sock, SOCKS_PORT as u32)
            .replace();
        for (k, v) in proxy_env("guest") {
            b = b.env(k, v);
        }
        // §21 G5: granted folders at /mnt/<name> (fixed at create; the kernel restarts the VM when grants change)
        for (guest, host) in extra_mounts(host_ws) {
            b = b.volume(guest, move |v| v.bind(host).stat_virtualization(StatVirtualization::Off));
        }
        let t_create = Instant::now();
        let sandbox = match b.create().await {
            Ok(s) => s,
            Err(e) => {
                for t in tasks {
                    t.abort();
                }
                remove_sockets(&run_dir);
                return Err(format!("could not boot the app VM: {e}"));
            }
        };
        let create_ms = t_create.elapsed().as_millis() as u64;
        prune_old_images(&env).await;
        let vm = AppVm {
            sandbox,
            name,
            host_ws: host_ws.to_path_buf(),
            run_dir,
            alive: alive.clone(),
            tasks: Mutex::new(tasks),
            guest_tools: Mutex::new(Value::Null),
            timings: Mutex::new(Value::Null),
        };
        // apt lists + .deb cache live on the volume (image apt.conf); the filesystem fills a grown disk
        let (code, _, err) = vm
            .sh(
                &format!(
                    "mkdir -p {GUEST_HOME} {GUEST_OPT}/runtimes {GUEST_OPT}/apt/lists/partial {GUEST_OPT}/apt/archives/partial /run/openapp/svc \
                     && {{ ! command -v resize2fs >/dev/null || [ \"$(df -Pk {GUEST_OPT} | awk 'NR==2{{print $2}}')\" -ge $(({OPT_VOLUME_MIB} * 1024 * 9 / 10)) ] || resize2fs \"$(awk '$2==\"{GUEST_OPT}\"{{print $1}}' /proc/mounts)\" >/dev/null 2>&1 || echo resize2fs failed >&2; }}"
                ),
                &[],
            )
            .await?;
        if code != 0 {
            return Err(format!("guest setup failed: {err}"));
        }
        if !err.trim().is_empty() {
            log::warn!("app VM volume: {}", err.trim());
        }
        let _ = grown;
        vm.write_file(VSOCK_RELAY_GUEST, VSOCK_RELAY_PY.as_bytes()).await?;
        let vm_arc = Arc::new(vm);
        for port in [MCP_PORT, PROXY_PORT, SOCKS_PORT] {
            let t = tokio::spawn(supervise_forwarder(Arc::downgrade(&vm_arc), alive.clone(), port));
            vm_arc.tasks.locked().push(t);
        }
        // wait until both guest listeners accept
        let probe = format!(
            "for i in $(seq 1 200); do (exec 3<>/dev/tcp/127.0.0.1/{MCP_PORT}) 2>/dev/null && (exec 3<>/dev/tcp/127.0.0.1/{PROXY_PORT}) 2>/dev/null && (exec 3<>/dev/tcp/127.0.0.1/{SOCKS_PORT}) 2>/dev/null && exit 0; sleep 0.025; done; exit 1"
        );
        let (code, _, _) = vm_arc.exec_out("bash", &["-c".into(), probe], &[], None).await?;
        if code != 0 {
            return Err("guest forwarders did not start (vsock relay)".into());
        }
        let (_, tools, _) = vm_arc
            .sh("for t in node npm python3 git curl ffmpeg socat rg fd bwrap apt-get gcc make Xvfb x11vnc websockify qemu-x86_64 proxychains4; do printf '%s=%s\\n' $t \"$(command -v $t || true)\"; done", &[])
            .await?;
        let mut m = serde_json::Map::new();
        for l in tools.lines() {
            if let Some((k, v)) = l.split_once('=') {
                m.insert(k.into(), if v.is_empty() { Value::Bool(false) } else { json!(v) });
            }
        }
        *vm_arc.guest_tools.locked() = Value::Object(m);
        *vm_arc.timings.locked() =
            json!({ "image_load_ms": image_ms, "create_ms": create_ms, "boot_total_ms": t0.elapsed().as_millis() as u64 });
        let vm = vm_arc;
        log::info!("app VM {} up in {} ms", vm.name, t0.elapsed().as_millis());
        Ok(vm)
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// Host path inside the workspace → guest path under /workspace.
    pub fn guest_path(&self, host: &Path) -> Option<String> {
        let rel = host.strip_prefix(&self.host_ws).ok()?;
        let rel = rel.to_string_lossy();
        Some(if rel.is_empty() { GUEST_WS.to_string() } else { format!("{GUEST_WS}/{rel}") })
    }

    /// Spawn a guest process with piped stdio.
    pub async fn spawn(&self, cmd: &str, args: &[String], env: &[(String, String)], cwd: Option<&str>) -> Result<GuestChild, String> {
        let args = args.to_vec();
        let env = env.to_vec();
        let cwd = cwd.map(String::from);
        let mut h = self
            .sandbox
            .exec_stream_with(cmd, move |e| {
                let mut e = e.args(args).stdin_pipe();
                if let Some(c) = cwd {
                    e = e.cwd(c);
                }
                for (k, v) in env {
                    e = e.env(k, v);
                }
                e
            })
            .await
            .map_err(|e| format!("guest exec {cmd}: {e}"))?;
        let control = h.control();
        let sink = h.take_stdin().ok_or("guest exec: no stdin")?;
        let (stdin_host, mut stdin_pump) = tokio::io::duplex(64 * 1024);
        let (mut stdout_pump, stdout_host) = tokio::io::duplex(1024 * 1024);
        let (mut stderr_pump, stderr_host) = tokio::io::duplex(256 * 1024);
        let (exit_tx, exit_rx) = oneshot::channel();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 32 * 1024];
            loop {
                match stdin_pump.read(&mut buf).await {
                    Ok(0) | Err(_) => {
                        let _ = sink.close().await;
                        break;
                    }
                    Ok(n) => {
                        if sink.write(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        tokio::spawn(async move {
            let mut status = ExitInfo { code: -1, failed: None };
            while let Some(ev) = h.recv().await {
                match ev {
                    ExecEvent::Stdout(d) => {
                        let _ = stdout_pump.write_all(&d).await;
                    }
                    ExecEvent::Stderr(d) => {
                        let _ = stderr_pump.write_all(&d).await;
                    }
                    ExecEvent::Exited { code } => {
                        status.code = code;
                        break;
                    }
                    ExecEvent::Failed(f) => {
                        let msg = format!("{f:?}");
                        let _ = stderr_pump.write_all(format!("spawn failed: {msg}\n").as_bytes()).await;
                        status.failed = Some(msg);
                        break;
                    }
                    _ => {}
                }
            }
            drop(stdout_pump);
            drop(stderr_pump);
            let _ = exit_tx.send(status);
        });
        Ok(GuestChild { stdin: Some(stdin_host), stdout: Some(stdout_host), stderr: Some(stderr_host), exit: Some(exit_rx), control })
    }

    /// §21 G2 PTY: start a guest process on a pseudo-terminal (stdin piped). The caller owns the
    /// event stream (`ExecEvent::Stdout` carries the terminal output) and resizes through `control()`.
    pub async fn exec_tty(&self, cmd: &str, args: &[String], env: &[(String, String)], cwd: Option<&str>) -> Result<microsandbox::ExecHandle, String> {
        let args = args.to_vec();
        let env = env.to_vec();
        let cwd = cwd.map(String::from);
        self.sandbox
            .exec_stream_with(cmd, move |e| {
                let mut e = e.args(args).stdin_pipe().tty(true);
                if let Some(c) = cwd {
                    e = e.cwd(c);
                }
                for (k, v) in env {
                    e = e.env(k, v);
                }
                e
            })
            .await
            .map_err(|e| format!("guest exec {cmd} (tty): {e}"))
    }

    /// A byte stream to guest `127.0.0.1:<port>` (one `socat` exec per stream, ~1 ms). Returns the
    /// child (stdin = to the guest port, stdout = from it; kill it when done).
    pub async fn connect_tcp(&self, port: u16) -> Result<GuestChild, String> {
        let mut c = self.spawn("socat", &["STDIO".into(), format!("TCP:127.0.0.1:{port}")], &[], None).await?;
        if let Some(mut se) = c.stderr.take() {
            tokio::spawn(async move {
                let mut s = String::new();
                let _ = se.read_to_string(&mut s).await;
                if !s.trim().is_empty() {
                    log::debug!("vm connect {port}: {}", s.trim());
                }
            });
        }
        Ok(c)
    }

    /// Run a program to completion; returns (exit code, stdout, stderr).
    pub async fn exec_out(&self, cmd: &str, args: &[String], env: &[(String, String)], cwd: Option<&str>) -> Result<(i32, String, String), String> {
        let args = args.to_vec();
        let env = env.to_vec();
        let cwd = cwd.map(String::from);
        let out = self
            .sandbox
            .exec_with(cmd, move |e| {
                let mut e = e.args(args).stdin_null();
                if let Some(c) = cwd {
                    e = e.cwd(c);
                }
                for (k, v) in env {
                    e = e.env(k, v);
                }
                e
            })
            .await
            .map_err(|e| format!("guest exec {cmd}: {e}"))?;
        Ok((out.status().code, out.stdout().unwrap_or_default(), out.stderr().unwrap_or_default()))
    }

    /// Run a shell snippet (`sh -c`).
    pub async fn sh(&self, script: &str, env: &[(String, String)]) -> Result<(i32, String, String), String> {
        self.exec_out("sh", &["-c".into(), script.into()], env, None).await
    }

    /// Write a file in the guest through the agent channel (visible to the guest at once, and to the
    /// host through the shared mount). Parent folders are created.
    pub async fn write_file(&self, guest_path: &str, data: &[u8]) -> Result<(), String> {
        if let Some((parent, _)) = guest_path.rsplit_once('/')
            && !parent.is_empty()
        {
            self.sandbox.fs().mkdir(parent).await.map_err(|e| format!("guest mkdir {parent}: {e}"))?;
        }
        self.sandbox.fs().write(guest_path, data).await.map_err(|e| format!("guest write {guest_path}: {e}"))
    }

    /// Forward host 127.0.0.1:`port` (and [::1]:`port` when free) to guest 127.0.0.1:`port`.
    pub async fn forward_host_port(self: &Arc<Self>, port: u16) -> Result<HostForward, String> {
        let v4 = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|e| format!("host port {port} is busy ({e})"))?;
        let mut listeners = vec![v4];
        if let Ok(v6) = tokio::net::TcpListener::bind(("::1", port)).await {
            listeners.push(v6);
        }
        let mut tasks = Vec::new();
        for l in listeners {
            let weak = Arc::downgrade(self);
            tasks.push(tokio::spawn(async move {
                while let Ok((tcp, _)) = l.accept().await {
                    let Some(vm) = weak.upgrade() else { break };
                    tokio::spawn(async move {
                        let child = vm.spawn("socat", &["STDIO".into(), format!("TCP:127.0.0.1:{port}")], &[], None).await;
                        let Ok(mut child) = child else { return };
                        let kill = child.killer();
                        let (mut tr, mut tw) = tcp.into_split();
                        let mut gin = child.take_stdin();
                        let mut gout = child.take_stdout();
                        let up = async move {
                            let _ = tokio::io::copy(&mut tr, &mut gin).await;
                            let _ = gin.shutdown().await;
                        };
                        let down = async move {
                            let _ = tokio::io::copy(&mut gout, &mut tw).await;
                            let _ = tw.shutdown().await;
                        };
                        tokio::join!(up, down);
                        kill.kill();
                    });
                }
            }));
        }
        Ok(HostForward { port, tasks })
    }

    /// Close the host sockets and forget them (sync; first step of quitting, so a slow or interrupted VM stop
    /// never leaves sockets that make the VM home look in use).
    pub fn close_sockets(&self) {
        self.alive.store(false, Ordering::Relaxed);
        for t in self.tasks.locked().drain(..) {
            t.abort();
        }
        remove_sockets(&self.run_dir);
    }

    /// Stop the VM (rootfs is discarded on the next boot; /opt/openapp and /workspace persist).
    pub async fn stop(&self) {
        self.close_sockets();
        if let Err(e) = self.sandbox.stop_with_timeout(Duration::from_secs(8)).await {
            log::warn!("app VM stop: {e}");
        }
    }
}

fn bind_unix(p: &Path) -> Result<tokio::net::UnixListener, String> {
    if p.to_string_lossy().len() > SOCKET_PATH_MAX {
        return Err(format!("socket path too long: {}", p.display()));
    }
    let _ = std::fs::remove_file(p);
    tokio::net::UnixListener::bind(p).map_err(|e| format!("bind {}: {e}", p.display()))
}

async fn accept_loop(l: tokio::net::UnixListener, handler: StreamHandler) {
    loop {
        match l.accept().await {
            Ok((s, _)) => handler(deframe(s)),
            Err(e) => {
                log::warn!("vm: accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

/// Keep the guest relay of vsock route `port` running (see the module docs).
async fn supervise_forwarder(vm: std::sync::Weak<AppVm>, alive: Arc<AtomicBool>, port: u16) {
    let args = vec![VSOCK_RELAY_GUEST.to_string(), port.to_string()];
    while alive.load(Ordering::Relaxed) {
        let Some(v) = vm.upgrade() else { return };
        let child = v.spawn("python3", &args, &[], None).await;
        drop(v);
        match child {
            Ok(mut c) => {
                // drain stderr so the pump never blocks
                if let Some(mut se) = c.stderr.take() {
                    tokio::spawn(async move {
                        let mut s = String::new();
                        let _ = se.read_to_string(&mut s).await;
                        if !s.trim().is_empty() {
                            log::warn!("vm forwarder {port}: {}", s.trim());
                        }
                    });
                }
                let code = c.wait().await;
                if alive.load(Ordering::Relaxed) {
                    log::warn!("vm forwarder {port} exited ({code}); restarting");
                }
            }
            Err(e) => log::warn!("vm forwarder {port}: {e}"),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// A host→guest TCP forward; dropping it closes the host listeners.
pub struct HostForward {
    pub port: u16,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for HostForward {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExitInfo {
    pub code: i32,
    /// The process never ran (binary not found, …).
    pub failed: Option<String>,
}

/// Kill handle for a guest process (cheap to clone; works from sync code inside a runtime).
#[derive(Clone)]
pub struct GuestKill(ExecControl);

impl GuestKill {
    pub fn kill(&self) {
        let c = self.0.clone();
        if let Ok(h) = tokio::runtime::Handle::try_current() {
            h.spawn(async move {
                let _ = c.kill().await;
            });
        }
    }
}

/// A guest process with host-side pipes, like a `tokio::process::Child`.
pub struct GuestChild {
    pub stdin: Option<DuplexStream>,
    pub stdout: Option<DuplexStream>,
    pub stderr: Option<DuplexStream>,
    exit: Option<oneshot::Receiver<ExitInfo>>,
    control: ExecControl,
}

impl GuestChild {
    pub fn take_stdin(&mut self) -> DuplexStream {
        self.stdin.take().expect("stdin taken")
    }
    pub fn take_stdout(&mut self) -> DuplexStream {
        self.stdout.take().expect("stdout taken")
    }
    pub fn killer(&self) -> GuestKill {
        GuestKill(self.control.clone())
    }
    /// Exit code (-1 when unknown or the process failed to start).
    pub async fn wait(&mut self) -> i32 {
        self.wait_info().await.code
    }
    pub async fn wait_info(&mut self) -> ExitInfo {
        match self.exit.take() {
            Some(rx) => rx.await.unwrap_or(ExitInfo { code: -1, failed: Some("exec session ended".into()) }),
            None => ExitInfo { code: -1, failed: Some("already waited".into()) },
        }
    }
    /// Split into the exit receiver (for a waiter task) and the rest.
    pub fn take_exit(&mut self) -> Option<oneshot::Receiver<ExitInfo>> {
        self.exit.take()
    }
}

/// Boxed stdio of a host or guest child.
pub type BoxRead = Box<dyn AsyncRead + Unpin + Send>;
pub type BoxWrite = Box<dyn AsyncWrite + Unpin + Send>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_paths() {
        let a = vm_name(Path::new("/Users/x/.openapp/workspace"));
        assert!(a.starts_with("oa-") && a.len() == 11, "{a}");
        assert_eq!(a, vm_name(Path::new("/Users/x/.openapp/workspace")));
        assert_ne!(a, vm_name(Path::new("/Users/x/other")));
        assert_eq!(proxy_url("agent"), "http://agent@127.0.0.1:7802");
        assert!(proxy_env("x").iter().any(|(k, v)| k == "NO_PROXY" && v.contains("127.0.0.1")));
        assert!(proxy_env("agent").iter().any(|(k, v)| k == "OPENAPP_SOCKS_URL" && v == "socks5h://agent:openapp@127.0.0.1:7803"));
        assert!(!proxy_env("agent").iter().any(|(k, _)| k.eq_ignore_ascii_case("ALL_PROXY")), "no global SOCKS ALL_PROXY");
        let w = socks_wrap("module.tg", &["python3".into(), "svc.py".into()]);
        assert_eq!((w[0].as_str(), w[3].as_str(), w[4].as_str(), w[5].as_str()), ("sh", "openapp-socks", "python3", "svc.py"));
        assert!(w[2].contains("socks5 127.0.0.1 7803 module.tg openapp") && w[2].contains("proxychains4 -q -f"));
        assert!(socks_wrap("a;rm -rf /", &[])[2].contains("arm-rf openapp"));
        assert!(wants_socks(&json!({ "net": "socks" })) && !wants_socks(&json!({})));
    }

    /// A socket left by a dead owner is stale and removed; a live listener of this process is reported.
    #[test]
    fn stale_sockets() {
        let home = std::env::temp_dir().join(format!("oa-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let (live, dead) = (home.join("run").join("oa-live"), home.join("run").join("oa-dead"));
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&dead).unwrap();
        let _l = std::os::unix::net::UnixListener::bind(live.join("proxy.sock")).unwrap();
        std::fs::write(live.join("owner.pid"), std::process::id().to_string()).unwrap();
        // a bound-then-dropped listener leaves a socket file nobody accepts on; the owner pid does not exist
        drop(std::os::unix::net::UnixListener::bind(dead.join("proxy.sock")).unwrap());
        std::fs::write(dead.join("mcp.sock"), "").unwrap();
        std::fs::write(dead.join("owner.pid"), "999999").unwrap();
        assert_eq!(running_vms(&home), vec!["oa-live".to_string()]);
        assert!(!dead.join("proxy.sock").exists() && !dead.join("mcp.sock").exists() && !dead.join("owner.pid").exists());
        assert!(live.join("proxy.sock").exists());
        let _ = std::fs::remove_dir_all(&home);
    }
}
