//! `UBRA_GPU_DIAG=1` retained profiling switch: every five seconds, print the
//! renderer's GPU-side memory to stderr so a process footprint can be
//! attributed (instance buffers, atlas textures, path targets, drawables,
//! frames submitted). Counters are relaxed atomics and cost nothing when the
//! switch is off; the reporter thread exists only when it is on.

use objc::{msg_send, sel, sel_impl};
use std::sync::{
    Once, OnceLock,
    atomic::{AtomicI64, AtomicU64, Ordering::Relaxed},
};

pub(crate) static FRAMES: AtomicU64 = AtomicU64::new(0);
pub(crate) static RENDERERS: AtomicI64 = AtomicI64::new(0);
pub(crate) static INSTANCE_BUFFERS_OUTSTANDING: AtomicI64 = AtomicI64::new(0);
pub(crate) static INSTANCE_BUFFERS_POOLED: AtomicI64 = AtomicI64::new(0);
pub(crate) static INSTANCE_BUFFER_SIZE: AtomicI64 = AtomicI64::new(0);
pub(crate) static SPRITES_PEAK: AtomicI64 = AtomicI64::new(0);
pub(crate) static QUADS_PEAK: AtomicI64 = AtomicI64::new(0);
pub(crate) static INSTANCE_BYTES_PEAK: AtomicI64 = AtomicI64::new(0);
pub(crate) static INSTANCE_BUFFERS_OUTSTANDING_PEAK: AtomicI64 = AtomicI64::new(0);
pub(crate) static ATLAS_MONO_TEXTURES: AtomicI64 = AtomicI64::new(0);
pub(crate) static ATLAS_MONO_BYTES: AtomicI64 = AtomicI64::new(0);
pub(crate) static ATLAS_POLY_TEXTURES: AtomicI64 = AtomicI64::new(0);
pub(crate) static ATLAS_POLY_BYTES: AtomicI64 = AtomicI64::new(0);
pub(crate) static PATH_TEXTURE_BYTES: AtomicI64 = AtomicI64::new(0);
pub(crate) static DRAWABLE_PIXELS: AtomicI64 = AtomicI64::new(0);

struct DeviceRef(metal::Device);
// The reporter only calls the thread-safe `currentAllocatedSize` getter.
unsafe impl Send for DeviceRef {}
unsafe impl Sync for DeviceRef {}
static DEVICE: OnceLock<DeviceRef> = OnceLock::new();

pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("UBRA_GPU_DIAG").is_some_and(|v| v != "0"))
}

pub(crate) fn outstanding_changed() {
    INSTANCE_BUFFERS_OUTSTANDING_PEAK
        .fetch_max(INSTANCE_BUFFERS_OUTSTANDING.load(Relaxed), Relaxed);
}

pub(crate) fn atlas_texture(kind: gpui::AtlasTextureKind, bytes: i64, sign: i64) {
    let (count, total) = match kind {
        gpui::AtlasTextureKind::Polychrome => (&ATLAS_POLY_TEXTURES, &ATLAS_POLY_BYTES),
        _ => (&ATLAS_MONO_TEXTURES, &ATLAS_MONO_BYTES),
    };
    count.fetch_add(sign, Relaxed);
    total.fetch_add(sign * bytes, Relaxed);
}

pub(crate) fn start(device: &metal::Device) {
    if !enabled() {
        return;
    }
    let _ = DEVICE.set(DeviceRef(device.clone()));
    static START: Once = Once::new();
    START.call_once(|| {
        std::thread::Builder::new()
            .name("ubra-gpu-diag".into())
            .spawn(report_loop)
            .ok();
    });
}

fn report_loop() {
    let mib = |bytes: i64| bytes as f64 / (1024.0 * 1024.0);
    let mut last_frames = 0;
    loop {
        std::thread::sleep(std::time::Duration::from_secs(5));
        let allocated: u64 = DEVICE.get().map_or(0, |device| unsafe {
            msg_send![device.0.as_ref(), currentAllocatedSize]
        });
        let frames = FRAMES.load(Relaxed);
        eprintln!(
            "gpu-diag device_allocated={:.1}MiB frames/5s={} renderers={} \
             instance_buffers: outstanding={} pooled={} size={:.1}MiB peak_outstanding={} frame_peak={:.2}MiB sprites_peak={} quads_peak={} \
             atlas_mono={} ({:.1}MiB) atlas_poly={} ({:.1}MiB) path_targets={:.1}MiB \
             drawable_px_total={}",
            allocated as f64 / (1024.0 * 1024.0),
            frames - last_frames,
            RENDERERS.load(Relaxed),
            INSTANCE_BUFFERS_OUTSTANDING.load(Relaxed),
            INSTANCE_BUFFERS_POOLED.load(Relaxed),
            mib(INSTANCE_BUFFER_SIZE.load(Relaxed)),
            INSTANCE_BUFFERS_OUTSTANDING_PEAK.load(Relaxed),
            mib(INSTANCE_BYTES_PEAK.swap(0, Relaxed)),
            SPRITES_PEAK.swap(0, Relaxed),
            QUADS_PEAK.swap(0, Relaxed),
            ATLAS_MONO_TEXTURES.load(Relaxed),
            mib(ATLAS_MONO_BYTES.load(Relaxed)),
            ATLAS_POLY_TEXTURES.load(Relaxed),
            mib(ATLAS_POLY_BYTES.load(Relaxed)),
            mib(PATH_TEXTURE_BYTES.load(Relaxed)),
            DRAWABLE_PIXELS.load(Relaxed),
        );
        last_frames = frames;
    }
}
