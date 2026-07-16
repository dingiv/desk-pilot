//! Parallel SoM-annotation bench. Runs the Rust OmniParser over a batch of images
//! across N worker threads — **each owning its own YOLO session** (true
//! parallelism, not a shared lock) — optionally on CUDA, and reports per-image
//! latency + throughput.
//!
//! CPU:
//!   cargo run -p vrover-omniparser --example bench -- <imgs_dir> --workers 4
//! CUDA (needs `--features cuda` + CUDA/cuDNN on the host):
//!   cargo run -p vrover-omniparser --example bench --features cuda -- <imgs_dir> --device cuda --workers 2
//!
//! Flags: `--device cpu|cuda`  `--workers N`  `--repeat K` (duplicate the set)
//!        `--out DIR` (write each annotated SoM PNG there)

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use vrover_omniparser::{CudaOptions, Device, OmniParser, OmniParserConfig, TensorRtxOptions};

fn main() {
    let mut args = std::env::args().skip(1);
    let root = match args.next() {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!(
                "usage: bench <imgs_dir_or_file> [--device cpu|cuda] [--workers N] [--repeat K] [--out DIR]"
            );
            std::process::exit(1);
        }
    };

    let mut device = Device::Cpu;
    let mut workers: usize = 1;
    let mut repeat: usize = 1;
    let mut out: Option<PathBuf> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--device" => {
                device = match args.next().as_deref() {
                    Some("cuda") => Device::Cuda(CudaOptions::default()),
                    Some("tensorrt-rtx") | Some("tensorrt_rtx") | Some("nvrtx") => {
                        Device::TensorRtx(TensorRtxOptions::default())
                    }
                    _ => Device::Cpu,
                };
            }
            "--workers" => workers = args.next().and_then(|s| s.parse().ok()).unwrap_or(1),
            "--repeat" => repeat = args.next().and_then(|s| s.parse().ok()).unwrap_or(1),
            "--out" => out = args.next().map(PathBuf::from),
            other => eprintln!("[bench] ignoring unknown arg: {other}"),
        }
    }
    workers = workers.max(1);

    // ort silently falls back to CPU if a GPU EP can't initialize (e.g. cuDNN /
    // CUDA-runtime libs missing from the linker path). Probe ldconfig AND
    // LD_LIBRARY_PATH so we warn loudly instead of reporting misleading
    // "device=…" timings that actually ran on CPU.
    if matches!(device, Device::Cuda(_) | Device::TensorRtx(_)) && !cuda_libs_present() {
        eprintln!(
            "[bench] WARNING: --device {} requested, but CUDA runtime/cuDNN libs were not found on \
the linker path (ldconfig ∪ LD_LIBRARY_PATH). ort will silently fall back to CPU here — the numbers \
below are NOT GPU. Point LD_LIBRARY_PATH at a CUDA/cuDNN install (e.g. a torch `nvidia/*/lib` tree).",
            device.label()
        );
    }

    // Collect images.
    let imgs = collect_pngs(&root);
    if imgs.is_empty() {
        eprintln!("[bench] no .png images under {}", root.display());
        std::process::exit(2);
    }
    let unique = imgs.len();
    let total = unique * repeat;
    eprintln!(
        "[bench] {unique} image(s) × {repeat} = {total} runs | device={} | workers={workers}",
        device.label()
    );

    let weights = std::env::var("OMNIPARSER_WEIGHTS").unwrap_or_else(|_| {
        format!("{}/weights/icon_detect.onnx", env!("CARGO_MANIFEST_DIR"))
    });

    // Work list = each image repeated. Shared queue + latency collector.
    let work: VecDeque<PathBuf> = (0..repeat).flat_map(|_| imgs.clone()).collect();
    let queue = Mutex::new(work);
    let latencies = Mutex::new(Vec::<u64>::with_capacity(total));
    let intra = if workers > 1 { Some(1) } else { None };

    let wall = Instant::now();
    let weights_ref = &weights;
    let queue_ref = &queue;
    let lat_ref = &latencies;
    std::thread::scope(|s| {
        for _wid in 0..workers {
            let device = device.clone();
            let out_dir = out.as_ref();
            s.spawn(move || {
                let mut cfg = OmniParserConfig::new(weights_ref);
                cfg.device = device.clone();
                cfg.intra_threads = intra;
                let mut parser = match OmniParser::new(cfg) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!(
                            "[bench] session init failed (device={}): {e}",
                            device.label()
                        );
                        std::process::exit(3);
                    }
                };
                loop {
                    // Pop next image (brief lock; parse runs unlocked).
                    let path = queue_ref.lock().unwrap().pop_front();
                    let Some(path) = path else { break };

                    let img = match image::open(&path) {
                        Ok(d) => d.to_rgb8(),
                        Err(e) => {
                            eprintln!("[bench] open {}: {e}", path.display());
                            continue;
                        }
                    };
                    let t0 = Instant::now();
                    let res = match parser.parse(&img) {
                        Ok(r) => r,
                        Err(e) => {
                            eprintln!("[bench] parse {}: {e}", path.display());
                            continue;
                        }
                    };
                    let us = t0.elapsed().as_micros() as u64;

                    if let Some(dir) = out_dir {
                        let _ = std::fs::create_dir_all(dir);
                        let stem = path
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "img".into());
                        let _ = std::fs::write(dir.join(format!("{stem}_som.png")), &res.annotated_png);
                    }
                    lat_ref.lock().unwrap().push(us);
                }
            });
        }
    });
    let elapsed = wall.elapsed();

    // Summary.
    let mut lat: Vec<u64> = latencies.into_inner().unwrap();
    if lat.is_empty() {
        eprintln!("[bench] no successful parses");
        std::process::exit(4);
    }
    lat.sort_unstable();
    let n = lat.len();
    let mean = lat.iter().sum::<u64>() as f64 / n as f64;
    let pct = |q: f64| -> f64 {
        lat[((n as f64 * q).floor() as usize).min(n - 1)] as f64
    };
    let secs = elapsed.as_secs_f64().max(1e-9);
    eprintln!(
        "[bench] done: {n} parsed in {secs:.2}s ({:.1} img/s) | per-image mean {:.2}ms  p50 {:.2}ms  p95 {:.2}ms  (device={})",
        n as f64 / secs,
        mean / 1000.0,
        pct(0.50) / 1000.0,
        pct(0.95) / 1000.0,
        device.label()
    );
}

/// Are CUDA-runtime + cuDNN libs visible to the dynamic linker (ldconfig cache ∪
/// `LD_LIBRARY_PATH` dirs)? ort needs both to engage a GPU EP; without them it
/// silently uses CPU. torch's `nvidia-*` wheels split them across dirs, so we look
/// for each soname in *any* candidate dir.
fn cuda_libs_present() -> bool {
    let (mut has_cudnn, mut has_cudart) = (false, false);
    if let Ok(out) = std::process::Command::new("ldconfig").arg("-p").output() {
        let s = String::from_utf8_lossy(&out.stdout);
        has_cudnn |= s.contains("libcudnn");
        has_cudart |= s.contains("libcudart");
    }
    if let Ok(ldp) = std::env::var("LD_LIBRARY_PATH") {
        for d in ldp.split(':').filter(|d| !d.is_empty()) {
            if let Ok(rd) = std::fs::read_dir(d) {
                for e in rd.flatten() {
                    let n = e.file_name().to_string_lossy().into_owned();
                    if n.starts_with("libcudnn") {
                        has_cudnn = true;
                    }
                    if n.starts_with("libcudart") {
                        has_cudart = true;
                    }
                }
            }
        }
    }
    has_cudnn && has_cudart
}

/// All `.png` paths under `root` (one level; pass a file to use just it).
fn collect_pngs(root: &Path) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_path_buf()];
    }
    let Ok(rd) = std::fs::read_dir(root) else {
        return vec![];
    };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|s| s.eq_ignore_ascii_case("png"))
                .unwrap_or(false)
        })
        .collect();
    v.sort();
    v
}
