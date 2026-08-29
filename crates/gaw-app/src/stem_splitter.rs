use std::{
    fmt::Write as _,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};

#[cfg(unix)]
use std::os::unix::process::CommandExt as _;

const XLANCE_EXECUTABLE_ENV: &str = "GAW_XLANCE";
const XLANCE_PYTHON_ENV: &str = "GAW_XLANCE_PYTHON";
const XLANCE_UV_ENV: &str = "GAW_UV";
const DEFAULT_XLANCE_TIMEOUT: Duration = Duration::from_hours(6);
const XLANCE_SETUP_TIMEOUT: Duration = Duration::from_hours(2);
const XLANCE_SETUP_RESERVE: u64 = 12 * 1024 * 1024 * 1024;
const BUNDLED_ADAPTER: &str = include_str!("../../../scripts/gaw-xlance");
const BUNDLED_CUDA_REQUIREMENTS: &str =
    include_str!("../../../scripts/xlance-requirements-linux.lock");
const BUNDLED_ROCM_GFX1010_REQUIREMENTS: &str =
    include_str!("../../../scripts/xlance-requirements-rocm-gfx1010.lock");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ManagedBackend {
    CudaOrCpu,
    RocmGfx1010,
}

#[derive(Debug)]
struct ManagedPython {
    executable: PathBuf,
    backend: ManagedBackend,
}

impl ManagedBackend {
    const fn id(self) -> &'static str {
        match self {
            Self::CudaOrCpu => "cuda-or-cpu",
            Self::RocmGfx1010 => "rocm-gfx1010",
        }
    }

    const fn requirements(self) -> &'static str {
        match self {
            Self::CudaOrCpu => BUNDLED_CUDA_REQUIREMENTS,
            Self::RocmGfx1010 => BUNDLED_ROCM_GFX1010_REQUIREMENTS,
        }
    }

    const fn device_override(self) -> Option<&'static str> {
        match self {
            Self::CudaOrCpu => None,
            Self::RocmGfx1010 => Some("rocm"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Stem {
    Vocals,
    Guitars,
    Keyboards,
    Bass,
    Synthesizers,
    Drums,
    Percussions,
    Orchestral,
}

impl Stem {
    pub(crate) const ALL: [Self; 8] = [
        Self::Vocals,
        Self::Guitars,
        Self::Keyboards,
        Self::Bass,
        Self::Synthesizers,
        Self::Drums,
        Self::Percussions,
        Self::Orchestral,
    ];

    pub(crate) const fn key(self) -> &'static str {
        match self {
            Self::Vocals => "vox",
            Self::Guitars => "gtr",
            Self::Keyboards => "key",
            Self::Bass => "bass",
            Self::Synthesizers => "syn",
            Self::Drums => "drums",
            Self::Percussions => "perc",
            Self::Orchestral => "orch",
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Vocals => "VOCALS",
            Self::Guitars => "GUITARS",
            Self::Keyboards => "KEYBOARDS",
            Self::Bass => "BASS",
            Self::Synthesizers => "SYNTHESIZERS",
            Self::Drums => "DRUMS",
            Self::Percussions => "PERCUSSIONS",
            Self::Orchestral => "ORCHESTRAL ELEMENTS",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct StemSplitOptions {
    pub stems: Vec<Stem>,
    pub denoise: bool,
    pub dereverb_vocals: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct StemSplitJob {
    pub asset_id: gaw_core::AssetId,
    pub content_hash: Option<String>,
    pub source_path: PathBuf,
    pub workspace_root: PathBuf,
    pub source_name: String,
    pub options: StemSplitOptions,
    pub cancelled: Arc<AtomicBool>,
    pub completed_stems: Arc<AtomicUsize>,
    pub installing: Arc<AtomicBool>,
}

#[derive(Debug)]
pub(crate) struct StemFile {
    pub stem: Stem,
    pub path: PathBuf,
}

#[derive(Debug)]
pub(crate) struct StemSplitOutput {
    _directory: tempfile::TempDir,
    pub files: Vec<StemFile>,
}

#[cfg(test)]
impl StemSplitOutput {
    pub(crate) fn from_test_files(files: &[(Stem, &Path)]) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let files = files
            .iter()
            .map(|(stem, source)| {
                let path = directory.path().join(format!("{}.wav", stem.key()));
                fs::copy(source, &path).unwrap();
                StemFile { stem: *stem, path }
            })
            .collect();
        Self {
            _directory: directory,
            files,
        }
    }
}

#[derive(Debug)]
pub(crate) struct StemSplitResult {
    pub job: StemSplitJob,
    pub output: Result<StemSplitOutput, String>,
}

pub(crate) fn split(job: &StemSplitJob, cancelled: &AtomicBool) -> Result<StemSplitOutput, String> {
    #[cfg(not(target_os = "linux"))]
    return Err("the bundled X-LANCE integration currently requires Linux".into());

    #[cfg(target_os = "linux")]
    split_with_command(job, cancelled, None)
}

#[cfg(test)]
fn split_with_executable(
    job: &StemSplitJob,
    cancelled: &AtomicBool,
    executable: &Path,
) -> Result<StemSplitOutput, String> {
    split_with_command(job, cancelled, Some(executable))
}

fn split_with_command(
    job: &StemSplitJob,
    cancelled: &AtomicBool,
    executable: Option<&Path>,
) -> Result<StemSplitOutput, String> {
    if job.options.stems.is_empty() {
        return Err("select at least one X-LANCE stem".into());
    }
    if is_cancelled(job, cancelled) {
        return Err("X-LANCE stem split was cancelled".into());
    }
    fs::create_dir_all(&job.workspace_root).map_err(|error| {
        format!(
            "could not create the X-LANCE workspace {}: {error}",
            job.workspace_root.display()
        )
    })?;
    preflight_workspace(job)?;
    let directory = tempfile::Builder::new()
        .prefix("job-")
        .tempdir_in(&job.workspace_root)
        .map_err(|error| error.to_string())?;
    let mut command = adapter_command(directory.path(), executable, job, cancelled)?;
    let stems = job
        .options
        .stems
        .iter()
        .map(|stem| stem.key())
        .collect::<Vec<_>>()
        .join(",");
    command
        .arg("split")
        .arg("--input")
        .arg(&job.source_path)
        .arg("--output-dir")
        .arg(directory.path())
        .arg("--stems")
        .arg(stems)
        .env("TMPDIR", directory.path())
        .env("TMP", directory.path())
        .env("TEMP", directory.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if job.options.denoise {
        command.arg("--denoise");
    }
    if job.options.dereverb_vocals && job.options.stems.contains(&Stem::Vocals) {
        command.arg("--dereverb-vocals");
    }
    run_command(&mut command, job, cancelled)?;

    let files = job
        .options
        .stems
        .iter()
        .map(|stem| {
            let path = directory.path().join(format!("{}.wav", stem.key()));
            if !path.is_file() {
                return Err(format!("X-LANCE did not produce the {} stem", stem.label()));
            }
            fs::canonicalize(&path)
                .map(|path| StemFile { stem: *stem, path })
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(StemSplitOutput {
        _directory: directory,
        files,
    })
}

fn run_command(
    command: &mut Command,
    job: &StemSplitJob,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!(
                "X-LANCE is not installed. Install its Python dependencies and set {XLANCE_PYTHON_ENV} to that environment's Python, or set {XLANCE_EXECUTABLE_ENV} to a compatible adapter."
            )
        } else {
            format!("could not start X-LANCE: {error}")
        }
    })?;
    let stdout = child.stdout.take();
    let completed_stems = Arc::clone(&job.completed_stems);
    let stdout_reader = thread::spawn(move || {
        stdout.map_or_else(Vec::new, |stdout| {
            read_progress_output(stdout, &completed_stems)
        })
    });
    let stderr = child.stderr.take();
    let stderr_reader = thread::spawn(move || stderr.map_or_else(Vec::new, read_bounded_output));
    let started = Instant::now();
    let timeout = xlance_timeout();
    let status = loop {
        if is_cancelled(job, cancelled) {
            terminate_process_tree(&mut child);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err("X-LANCE stem split was cancelled".into());
        }
        if started.elapsed() >= timeout {
            terminate_process_tree(&mut child);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(format!(
                "X-LANCE stem split timed out after {} hours",
                timeout.as_secs() / 3_600
            ));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(100)),
            Err(error) => {
                terminate_process_tree(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!("could not wait for X-LANCE: {error}"));
            }
        }
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    if !status.success() {
        let detail = last_output_line(&stderr)
            .or_else(|| last_output_line(&stdout))
            .unwrap_or("X-LANCE exited without an error message");
        return Err(format!("X-LANCE failed: {detail}"));
    }
    Ok(())
}

fn preflight_workspace(job: &StemSplitJob) -> Result<(), String> {
    const RESERVE: u64 = 512 * 1024 * 1024;
    let source_bytes = fs::metadata(&job.source_path)
        .map_err(|error| format!("could not inspect X-LANCE input: {error}"))?
        .len();
    let copies = u64::try_from(job.options.stems.len())
        .unwrap_or(u64::MAX)
        .saturating_mul(2)
        .saturating_add(3);
    let required = source_bytes.saturating_mul(copies).saturating_add(RESERVE);
    let available = fs2::available_space(&job.workspace_root)
        .map_err(|error| format!("could not check X-LANCE workspace capacity: {error}"))?;
    if available < required {
        let required_tenths = required.saturating_mul(10) / 1024_u64.pow(3);
        let available_tenths = available.saturating_mul(10) / 1024_u64.pow(3);
        return Err(format!(
            "X-LANCE needs about {}.{} GiB of free project storage for this split, but only {}.{} GiB is available",
            required_tenths / 10,
            required_tenths % 10,
            available_tenths / 10,
            available_tenths % 10,
        ));
    }
    Ok(())
}

fn is_cancelled(job: &StemSplitJob, worker_cancelled: &AtomicBool) -> bool {
    worker_cancelled.load(Ordering::Acquire) || job.cancelled.load(Ordering::Acquire)
}

fn xlance_timeout() -> Duration {
    std::env::var("GAW_XLANCE_TIMEOUT_HOURS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|hours| *hours > 0)
        .and_then(|hours| hours.checked_mul(3_600))
        .map_or(DEFAULT_XLANCE_TIMEOUT, Duration::from_secs)
}

fn terminate_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    // SAFETY: the child was placed in a new process group whose ID is its PID.
    // Sending SIGKILL to the negative ID targets only that group.
    unsafe {
        let _ = libc::kill(-child.id().cast_signed(), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = child.kill();
    let _ = child.wait();
}

fn adapter_command(
    directory: &Path,
    executable: Option<&Path>,
    job: &StemSplitJob,
    cancelled: &AtomicBool,
) -> Result<Command, String> {
    if let Some(executable) = executable
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os(XLANCE_EXECUTABLE_ENV).map(PathBuf::from))
    {
        Ok(Command::new(executable))
    } else {
        let (python, managed_backend) = if let Some(python) = std::env::var_os(XLANCE_PYTHON_ENV) {
            (PathBuf::from(python), None)
        } else {
            job.installing.store(true, Ordering::Release);
            let result = managed_xlance_python(job, cancelled);
            job.installing.store(false, Ordering::Release);
            let runtime = result?;
            (runtime.executable, Some(runtime.backend))
        };
        let adapter = directory.join("gaw-xlance");
        fs::write(&adapter, BUNDLED_ADAPTER).map_err(|error| {
            format!("could not materialize the bundled X-LANCE adapter: {error}")
        })?;
        let mut command = Command::new(python);
        command.arg(adapter);
        if let Some(backend) = managed_backend {
            configure_inference_command(&mut command);
            if let Some(device) = backend.device_override() {
                command.env("GAW_XLANCE_DEVICE", device);
            }
        }
        Ok(command)
    }
}

fn managed_xlance_python(
    job: &StemSplitJob,
    cancelled: &AtomicBool,
) -> Result<ManagedPython, String> {
    let backend = managed_backend()?;
    let data_root = app_data_root()?;
    let cache_root = app_cache_root()?;
    let runtime_base = std::env::var_os("GAW_XLANCE_RUNTIME_ROOT")
        .map_or_else(|| data_root.join("runtimes/xlance"), PathBuf::from)
        .join(backend.id());
    let fingerprint = runtime_fingerprint(backend);
    let runtime = runtime_base.join(&fingerprint);
    let python = runtime.join(".venv/bin/python");
    let ready = runtime.join("ready");
    if runtime_is_ready(&ready, &python, &fingerprint) {
        return Ok(ManagedPython {
            executable: python,
            backend,
        });
    }

    fs::create_dir_all(&runtime_base).map_err(|error| {
        format!(
            "could not create the X-LANCE runtime directory {}: {error}",
            runtime_base.display()
        )
    })?;
    preflight_runtime(&runtime_base)?;
    let lock_path = runtime_base.join("install.lock");
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| format!("could not open the X-LANCE install lock: {error}"))?;
    loop {
        match fs2::FileExt::try_lock_exclusive(&lock) {
            Ok(()) => break,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if is_cancelled(job, cancelled) {
                    return Err("X-LANCE installation was cancelled".into());
                }
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(format!("could not lock the X-LANCE runtime: {error}")),
        }
    }
    if runtime_is_ready(&ready, &python, &fingerprint) {
        return Ok(ManagedPython {
            executable: python,
            backend,
        });
    }
    remove_incomplete_runtime(&runtime)?;
    fs::create_dir_all(&runtime)
        .map_err(|error| format!("could not create the X-LANCE runtime: {error}"))?;
    let requirements = runtime.join("requirements.lock");
    fs::write(&requirements, backend.requirements())
        .map_err(|error| format!("could not write the X-LANCE dependency lock: {error}"))?;

    let uv = std::env::var_os(XLANCE_UV_ENV).map_or_else(|| PathBuf::from("uv"), PathBuf::from);
    let python_install_root = data_root.join("python");
    let uv_cache = cache_root.join("uv");
    let mut create_venv = Command::new(&uv);
    create_venv
        .arg("venv")
        .arg(runtime.join(".venv"))
        .arg("--python")
        .arg("3.12")
        .arg("--managed-python")
        .arg("--no-config");
    configure_setup_command(&mut create_venv, &python_install_root, &uv_cache);
    run_setup_command(
        &mut create_venv,
        job,
        cancelled,
        "create the X-LANCE environment",
    )?;

    let mut install = Command::new(&uv);
    install
        .arg("pip")
        .arg("install")
        .arg("--python")
        .arg(&python)
        .arg("--requirement")
        .arg(&requirements)
        .arg("--require-hashes")
        .arg("--only-binary")
        .arg(":all:")
        .arg("--no-config");
    if backend == ManagedBackend::RocmGfx1010 {
        install.arg("--no-binary").arg("rocm");
    }
    configure_setup_command(&mut install, &python_install_root, &uv_cache);
    run_setup_command(&mut install, job, cancelled, "install X-LANCE")?;

    let mut verify = Command::new(&python);
    verify.arg("-c").arg(runtime_verifier(backend));
    configure_setup_command(&mut verify, &python_install_root, &uv_cache);
    run_setup_command(&mut verify, job, cancelled, "verify X-LANCE")?;

    publish_ready_marker(&runtime, &ready, &fingerprint)?;
    Ok(ManagedPython {
        executable: python,
        backend,
    })
}

fn publish_ready_marker(runtime: &Path, ready: &Path, fingerprint: &str) -> Result<(), String> {
    let mut marker = tempfile::NamedTempFile::new_in(runtime)
        .map_err(|error| format!("could not create the X-LANCE ready marker: {error}"))?;
    std::io::Write::write_all(&mut marker, fingerprint.as_bytes())
        .map_err(|error| format!("could not write the X-LANCE ready marker: {error}"))?;
    marker
        .as_file()
        .sync_all()
        .map_err(|error| format!("could not sync the X-LANCE ready marker: {error}"))?;
    marker
        .persist(ready)
        .map_err(|error| format!("could not publish the X-LANCE runtime: {}", error.error))?;
    Ok(())
}

fn app_data_root() -> Result<PathBuf, String> {
    if let Some(root) = std::env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(root).join("gaw"));
    }
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".local/share/gaw"))
        .ok_or_else(|| "could not locate GAW's data directory (HOME is unset)".into())
}

fn app_cache_root() -> Result<PathBuf, String> {
    if let Some(root) = std::env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(root).join("gaw"));
    }
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".cache/gaw"))
        .ok_or_else(|| "could not locate GAW's cache directory (HOME is unset)".into())
}

fn runtime_fingerprint(backend: ManagedBackend) -> String {
    let mut hash = Sha256::new();
    hash.update(b"gaw-xlance-runtime-v2\0linux\0python-3.12\0");
    hash.update(std::env::consts::ARCH.as_bytes());
    hash.update([0]);
    hash.update(backend.id().as_bytes());
    hash.update([0]);
    hash.update(BUNDLED_ADAPTER.as_bytes());
    hash.update(backend.requirements().as_bytes());
    let mut encoded = String::with_capacity(64);
    for byte in hash.finalize() {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

fn managed_backend() -> Result<ManagedBackend, String> {
    let preference = std::env::var("GAW_XLANCE_DEVICE").unwrap_or_else(|_| "auto".into());
    choose_managed_backend(
        &preference,
        Path::new("/dev/nvidiactl").exists(),
        host_has_rocm_gfx1010(),
    )
}

fn choose_managed_backend(
    preference: &str,
    has_nvidia: bool,
    has_rocm_gfx1010: bool,
) -> Result<ManagedBackend, String> {
    match preference.trim().to_ascii_lowercase().as_str() {
        "auto" if has_nvidia => Ok(ManagedBackend::CudaOrCpu),
        "auto" | "rocm" if has_rocm_gfx1010 => Ok(ManagedBackend::RocmGfx1010),
        "auto" | "cpu" | "cuda" => Ok(ManagedBackend::CudaOrCpu),
        "rocm" => Err(
            "GAW_XLANCE_DEVICE=rocm requires an accessible AMD gfx1010 GPU (/dev/kfd and its DRM render node)"
                .into(),
        ),
        _ => Err("GAW_XLANCE_DEVICE must be auto, cuda, rocm, or cpu".into()),
    }
}

fn host_has_rocm_gfx1010() -> bool {
    let nodes = Path::new("/sys/class/kfd/kfd/topology/nodes");
    if !Path::new("/dev/kfd").exists() {
        return false;
    }
    let Ok(entries) = fs::read_dir(nodes) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        let Ok(properties) = fs::read_to_string(entry.path().join("properties")) else {
            return false;
        };
        let values = parse_kfd_properties(&properties);
        values.get("vendor_id") == Some(&4098)
            && values.get("gfx_target_version") == Some(&100_100)
            && values.get("drm_render_minor").is_some_and(|minor| {
                Path::new("/dev/dri")
                    .join(format!("renderD{minor}"))
                    .exists()
            })
    })
}

fn parse_kfd_properties(contents: &str) -> std::collections::HashMap<&str, u64> {
    contents
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((fields.next()?, fields.next()?.parse().ok()?))
        })
        .collect()
}

fn runtime_verifier(backend: ManagedBackend) -> &'static str {
    match backend {
        ManagedBackend::CudaOrCpu => {
            "import beartype, einops, filelock, huggingface_hub, librosa, numpy, packaging, rotary_embedding_torch, soundfile, torch, tqdm, yaml; print(torch.__version__)"
        }
        ManagedBackend::RocmGfx1010 => {
            "import beartype, einops, filelock, huggingface_hub, librosa, numpy, packaging, rotary_embedding_torch, soundfile, torch, torch.nn.functional as F, tqdm, yaml; assert torch.version.hip is not None, 'PyTorch has no HIP runtime'; assert torch.cuda.is_available(), 'ROCm cannot access the GPU'; arch=torch.cuda.get_device_properties(0).gcnArchName; assert arch.startswith('gfx1010'), f'expected gfx1010, got {arch}'; x=torch.randn(1,2,4096,device='cuda'); F.conv1d(x,torch.randn(4,2,9,device='cuda')); torch.stft(x[0,0],256,return_complex=True); q=torch.randn(1,2,32,16,device='cuda'); F.scaled_dot_product_attention(q,q,q); torch.cuda.synchronize(); print(torch.__version__, arch)"
        }
    }
}

fn configure_inference_command(command: &mut Command) {
    for variable in [
        "PYTHONHOME",
        "PYTHONPATH",
        "PYTHONSTARTUP",
        "VIRTUAL_ENV",
        "CONDA_PREFIX",
        "CUDA_VISIBLE_DEVICES",
        "HIP_VISIBLE_DEVICES",
        "ROCR_VISIBLE_DEVICES",
        "HSA_OVERRIDE_GFX_VERSION",
        "LD_LIBRARY_PATH",
    ] {
        command.env_remove(variable);
    }
    command
        .env("PYTHONNOUSERSITE", "1")
        .env("PYTHONSAFEPATH", "1");
}

fn runtime_is_ready(ready: &Path, python: &Path, fingerprint: &str) -> bool {
    python.is_file() && fs::read_to_string(ready).is_ok_and(|contents| contents == fingerprint)
}

fn remove_incomplete_runtime(runtime: &Path) -> Result<(), String> {
    let Ok(metadata) = fs::symlink_metadata(runtime) else {
        return Ok(());
    };
    let result = if metadata.file_type().is_symlink() || metadata.is_file() {
        fs::remove_file(runtime)
    } else {
        fs::remove_dir_all(runtime)
    };
    result.map_err(|error| format!("could not replace an incomplete X-LANCE runtime: {error}"))
}

fn preflight_runtime(runtime_base: &Path) -> Result<(), String> {
    let available = fs2::available_space(runtime_base)
        .map_err(|error| format!("could not check X-LANCE installation capacity: {error}"))?;
    if available < XLANCE_SETUP_RESERVE {
        let available_tenths = available.saturating_mul(10) / 1024_u64.pow(3);
        return Err(format!(
            "X-LANCE needs about 12 GiB of free app storage for its runtime, but only {}.{} GiB is available",
            available_tenths / 10,
            available_tenths % 10,
        ));
    }
    Ok(())
}

fn configure_setup_command(command: &mut Command, python_root: &Path, cache_root: &Path) {
    for variable in [
        "PYTHONHOME",
        "PYTHONPATH",
        "PYTHONSTARTUP",
        "VIRTUAL_ENV",
        "CONDA_PREFIX",
        "PIP_CONFIG_FILE",
        "PIP_INDEX_URL",
        "PIP_EXTRA_INDEX_URL",
        "UV_CONFIG_FILE",
        "UV_INDEX",
        "UV_DEFAULT_INDEX",
        "UV_EXTRA_INDEX_URL",
    ] {
        command.env_remove(variable);
    }
    command
        .env("UV_PYTHON_INSTALL_DIR", python_root)
        .env("UV_CACHE_DIR", cache_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
}

fn run_setup_command(
    command: &mut Command,
    job: &StemSplitJob,
    cancelled: &AtomicBool,
    action: &str,
) -> Result<(), String> {
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!(
                "GAW needs uv to install X-LANCE automatically. Install uv or set {XLANCE_UV_ENV} to its executable."
            )
        } else {
            format!("could not {action}: {error}")
        }
    })?;
    let stdout = child.stdout.take();
    let stdout_reader = thread::spawn(move || stdout.map_or_else(Vec::new, read_bounded_output));
    let stderr = child.stderr.take();
    let stderr_reader = thread::spawn(move || stderr.map_or_else(Vec::new, read_bounded_output));
    let started = Instant::now();
    let status = loop {
        if is_cancelled(job, cancelled) {
            terminate_process_tree(&mut child);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err("X-LANCE installation was cancelled".into());
        }
        if started.elapsed() >= XLANCE_SETUP_TIMEOUT {
            terminate_process_tree(&mut child);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(format!(
                "X-LANCE installation timed out while trying to {action}"
            ));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(100)),
            Err(error) => {
                terminate_process_tree(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!("could not wait while trying to {action}: {error}"));
            }
        }
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    if !status.success() {
        let detail = last_output_line(&stderr)
            .or_else(|| last_output_line(&stdout))
            .unwrap_or("the installer exited without an error message");
        return Err(format!("could not {action}: {detail}"));
    }
    Ok(())
}

fn read_bounded_output(mut input: impl Read) -> Vec<u8> {
    const LIMIT: usize = 256 * 1024;
    let mut output = Vec::new();
    let mut chunk = [0_u8; 8 * 1024];
    while let Ok(read) = input.read(&mut chunk) {
        if read == 0 {
            break;
        }
        output.extend_from_slice(&chunk[..read]);
        if output.len() > LIMIT {
            output.drain(..output.len() - LIMIT);
        }
    }
    output
}

fn read_progress_output(input: impl Read, completed_stems: &AtomicUsize) -> Vec<u8> {
    const LIMIT: usize = 256 * 1024;
    let mut output = Vec::new();
    let mut reader = BufReader::new(input);
    let mut line = Vec::new();
    loop {
        line.clear();
        let Ok(read) = reader.read_until(b'\n', &mut line) else {
            break;
        };
        if read == 0 {
            break;
        }
        if serde_json::from_slice::<serde_json::Value>(&line)
            .ok()
            .and_then(|value| {
                value
                    .get("event")
                    .and_then(|event| event.as_str())
                    .map(str::to_owned)
            })
            .as_deref()
            == Some("stem_complete")
        {
            completed_stems.fetch_add(1, Ordering::Release);
        }
        output.extend_from_slice(&line);
        if output.len() > LIMIT {
            output.drain(..output.len() - LIMIT);
        }
    }
    output
}

fn last_output_line(output: &[u8]) -> Option<&str> {
    std::str::from_utf8(output)
        .ok()?
        .lines()
        .rev()
        .find_map(|line| (!line.trim().is_empty()).then_some(line.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn exposes_the_eight_xlance_targets_in_display_order() {
        assert_eq!(Stem::ALL.len(), 8);
        assert_eq!(Stem::ALL[0].label(), "VOCALS");
        assert_eq!(Stem::ALL[7].label(), "ORCHESTRAL ELEMENTS");
    }

    #[test]
    fn progress_reader_counts_completed_stems() {
        let completed = AtomicUsize::new(0);
        let output = read_progress_output(
            Cursor::new(b"loading\n{\"event\":\"stem_complete\",\"stem\":\"vox\"}\n".as_slice()),
            &completed,
        );
        assert_eq!(completed.load(Ordering::Acquire), 1);
        assert!(String::from_utf8(output).unwrap().contains("stem_complete"));
    }

    #[test]
    fn managed_runtime_is_visible_only_after_its_matching_ready_marker() {
        let directory = tempfile::tempdir().unwrap();
        let python = directory.path().join(".venv/bin/python");
        let ready = directory.path().join("ready");
        fs::create_dir_all(python.parent().unwrap()).unwrap();
        fs::write(&python, []).unwrap();
        assert!(!runtime_is_ready(&ready, &python, "expected"));
        fs::write(&ready, "different").unwrap();
        assert!(!runtime_is_ready(&ready, &python, "expected"));
        fs::write(&ready, "expected").unwrap();
        assert!(runtime_is_ready(&ready, &python, "expected"));
    }

    #[test]
    fn backend_selection_prefers_nvidia_then_gfx1010() {
        assert_eq!(
            choose_managed_backend("auto", true, true).unwrap(),
            ManagedBackend::CudaOrCpu
        );
        assert_eq!(
            choose_managed_backend("auto", false, true).unwrap(),
            ManagedBackend::RocmGfx1010
        );
        assert_eq!(
            choose_managed_backend("cpu", false, true).unwrap(),
            ManagedBackend::CudaOrCpu
        );
        assert_eq!(
            choose_managed_backend("rocm", false, true).unwrap(),
            ManagedBackend::RocmGfx1010
        );
        assert!(choose_managed_backend("rocm", false, false).is_err());
        assert!(choose_managed_backend("metal", false, false).is_err());
    }

    #[test]
    fn kfd_properties_identify_the_rx_5700_xt_target() {
        let properties = parse_kfd_properties(
            "cpu_cores_count 0\nsimd_count 80\nmem_banks_count 1\ngfx_target_version 100100\n\
             vendor_id 4098\ndrm_render_minor 128\n",
        );
        assert_eq!(properties.get("gfx_target_version"), Some(&100_100));
        assert_eq!(properties.get("vendor_id"), Some(&4098));
        assert_eq!(properties.get("drm_render_minor"), Some(&128));
    }

    #[test]
    fn managed_backends_have_distinct_runtime_fingerprints() {
        assert_ne!(
            runtime_fingerprint(ManagedBackend::CudaOrCpu),
            runtime_fingerprint(ManagedBackend::RocmGfx1010)
        );
    }

    #[cfg(unix)]
    #[test]
    fn incomplete_runtime_cleanup_does_not_follow_a_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let outside = directory.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), []).unwrap();
        let runtime = directory.path().join("runtime");
        symlink(&outside, &runtime).unwrap();

        remove_incomplete_runtime(&runtime).unwrap();

        assert!(!runtime.exists());
        assert!(outside.join("keep").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn executable_contract_collects_only_requested_outputs() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("fake-xlance");
        fs::write(
            &executable,
            "#!/bin/sh\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n    --output-dir) out=$2; shift 2 ;;\n    --stems) stems=$2; shift 2 ;;\n    *) shift ;;\n  esac\ndone\nmkdir -p \"$out\"\nprintf '%s\\n' \"$stems\" | tr ',' '\\n' | while read stem; do : > \"$out/$stem.wav\"; done\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        let source = directory.path().join("song.wav");
        fs::write(&source, []).unwrap();
        let job = StemSplitJob {
            asset_id: gaw_core::AssetId::new(),
            content_hash: None,
            source_path: source,
            workspace_root: directory.path().join("workspace"),
            source_name: "song.wav".into(),
            options: StemSplitOptions {
                stems: vec![Stem::Vocals, Stem::Drums],
                denoise: true,
                dereverb_vocals: true,
            },
            cancelled: Arc::new(AtomicBool::new(false)),
            completed_stems: Arc::new(AtomicUsize::new(0)),
            installing: Arc::new(AtomicBool::new(false)),
        };
        let output = split_with_executable(&job, &AtomicBool::new(false), &executable).unwrap();
        assert_eq!(output.files.len(), 2);
        assert_eq!(output.files[0].stem, Stem::Vocals);
        assert_eq!(output.files[1].stem, Stem::Drums);
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_terminates_the_adapter_process_group() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("slow-xlance");
        fs::write(&executable, "#!/bin/sh\nsleep 30 &\nwait\n").unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        let source = directory.path().join("song.wav");
        fs::write(&source, []).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let job = StemSplitJob {
            asset_id: gaw_core::AssetId::new(),
            content_hash: None,
            source_path: source,
            workspace_root: directory.path().join("workspace"),
            source_name: "song.wav".into(),
            options: StemSplitOptions {
                stems: vec![Stem::Vocals],
                denoise: false,
                dereverb_vocals: false,
            },
            cancelled: Arc::clone(&cancelled),
            completed_stems: Arc::new(AtomicUsize::new(0)),
            installing: Arc::new(AtomicBool::new(false)),
        };
        let started = Instant::now();
        let worker_cancelled = Arc::new(AtomicBool::new(false));
        let handle =
            thread::spawn(move || split_with_executable(&job, &worker_cancelled, &executable));
        thread::sleep(Duration::from_millis(200));
        cancelled.store(true, Ordering::Release);

        assert!(handle.join().unwrap().unwrap_err().contains("cancelled"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
