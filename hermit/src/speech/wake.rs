//! Wake-word detection.
//!
//! # Why this binds the C library directly
//!
//! The spec calls for Picovoice Porcupine, and Porcupine is still the right engine
//! here — it is tiny, accurate, and runs in well under 1% of a Pi 4 core. But there
//! is no usable Rust crate any more:
//!
//! - `pv_porcupine` on crates.io is a reserved placeholder at version 0.0.0.
//! - `Picovoice/porcupine` no longer ships a `binding/rust` directory at all
//!   (the bindings that remain are android/dotnet/flutter/ios/java/nodejs/python/
//!   react/react-native/web).
//!
//! So we bind the stable C ABI ourselves. It is a five-function API and the shape
//! has not changed across major versions.
//!
//! Loading happens at runtime with `dlopen` rather than at link time. That is a
//! deliberate robustness choice for an appliance: a missing or mismatched
//! `libpv_porcupine.so` degrades to "wake word disabled, text still works" instead
//! of a binary that will not start. It also keeps cross-compilation free of any
//! Picovoice artifacts.
//!
//! Detection runs on the XVF3800's *processed* channel, which has already had AEC,
//! beamforming, noise suppression and dereverberation applied in hardware — which
//! is why accuracy holds up while music is playing.

#[cfg(feature = "wake-porcupine")]
use anyhow::Context;
#[cfg(feature = "wake-porcupine")]
use anyhow::{Result, bail};

/// Frames Porcupine expects per `process` call. Fixed by the library at 512 for a
/// 16 kHz pipeline; queried at runtime and asserted rather than assumed.
pub const EXPECTED_SAMPLE_RATE: u32 = 16_000;

/// What a wake-word engine must do.
pub trait WakeDetector: Send {
    /// Samples per `process` call.
    fn frame_length(&self) -> usize;
    /// Returns the index of the keyword detected in this frame, if any.
    fn process(&mut self, frame: &[i16]) -> Option<usize>;
    /// Most recent detection score (0..1) and the firing threshold, for telemetry.
    /// Engines without a meaningful score (Porcupine, Null) return None.
    fn last_score(&self) -> Option<(f32, f32)> {
        None
    }
    /// Adjust the firing threshold at runtime (feedback tuning). Engines with
    /// no tunable threshold ignore it.
    fn set_threshold(&mut self, _threshold: f32) {}
}

/// Boxed detectors are detectors too — `build()` returns a trait object and the
/// feeder is generic, so without this the two cannot meet.
impl WakeDetector for Box<dyn WakeDetector> {
    fn frame_length(&self) -> usize {
        (**self).frame_length()
    }
    fn process(&mut self, frame: &[i16]) -> Option<usize> {
        (**self).process(frame)
    }
    fn last_score(&self) -> Option<(f32, f32)> {
        (**self).last_score()
    }
    fn set_threshold(&mut self, threshold: f32) {
        (**self).set_threshold(threshold)
    }
}

/// Always-silent detector, used when wake word is disabled or unavailable.
pub struct NullWake {
    frame_length: usize,
}

impl NullWake {
    pub fn new() -> Self {
        Self { frame_length: 512 }
    }
}

impl Default for NullWake {
    fn default() -> Self {
        Self::new()
    }
}

impl WakeDetector for NullWake {
    fn frame_length(&self) -> usize {
        self.frame_length
    }
    fn process(&mut self, _frame: &[i16]) -> Option<usize> {
        None
    }
}

/// Feeds a continuous sample stream to a detector in exact frame-sized bites.
///
/// ALSA hands us periods that have nothing to do with Porcupine's 512-sample frame,
/// so this buffers the remainder across reads. Getting this wrong is the classic
/// way to end up with a wake word that only fires occasionally.
/// 2.5 s at 16 kHz.
const RECENT_SAMPLES: usize = 40_000;

pub struct FrameFeeder<D: WakeDetector> {
    detector: D,
    buffer: Vec<i16>,
    /// Rolling copy of the last ~2.5 s of raw mic audio. When the wake word
    /// fires, this IS the evidence - persisted (on user confirmation) as a
    /// labeled clip for the Indic retrain dataset.
    recent: std::collections::VecDeque<i16>,
}

impl<D: WakeDetector> FrameFeeder<D> {
    pub fn new(detector: D) -> Self {
        let cap = detector.frame_length() * 4;
        Self {
            detector,
            buffer: Vec::with_capacity(cap),
            recent: std::collections::VecDeque::with_capacity(RECENT_SAMPLES),
        }
    }

    /// Push arbitrary-length audio; returns the keyword index if one fires.
    pub fn push(&mut self, samples: &[i16]) -> Option<usize> {
        for &s in samples {
            if self.recent.len() == RECENT_SAMPLES {
                self.recent.pop_front();
            }
            self.recent.push_back(s);
        }
        self.buffer.extend_from_slice(samples);
        let n = self.detector.frame_length();
        if n == 0 {
            return None;
        }
        let mut hit = None;
        let mut consumed = 0;
        while self.buffer.len() - consumed >= n {
            let frame = &self.buffer[consumed..consumed + n];
            if let Some(idx) = self.detector.process(frame) {
                hit = Some(idx);
                consumed += n;
                break;
            }
            consumed += n;
        }
        if consumed > 0 {
            self.buffer.drain(..consumed);
        }
        hit
    }

    /// Drop buffered audio (e.g. after a detection, so the keyword is not re-seen).
    pub fn reset(&mut self) {
        self.buffer.clear();
    }

    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }

    /// Telemetry passthrough to the wrapped detector.
    pub fn last_score(&self) -> Option<(f32, f32)> {
        self.detector.last_score()
    }

    /// Runtime threshold override (feedback tuning).
    pub fn set_threshold(&mut self, threshold: f32) {
        self.detector.set_threshold(threshold);
    }

    /// The last ~2.5 s of raw audio - the window that contains the wake word
    /// when called right after a detection. Copied out so the feeder can keep
    /// rolling while the caller persists it.
    pub fn recent_audio(&self) -> Vec<i16> {
        self.recent.iter().copied().collect()
    }
}

/// Build the configured detector, degrading to [`NullWake`] with a clear log line
/// rather than failing the daemon's start-up.
pub fn build(cfg: &crate::config::Wake) -> Box<dyn WakeDetector> {
    if !cfg.enabled {
        tracing::info!("wake word disabled by config");
        return Box::new(NullWake::new());
    }

    // Prefer the project's own "Hey Sudo" model when its ONNX files are present.
    // It is the trained, in-house wake word; Porcupine is only a fallback for a box
    // that has a Picovoice key but not the models.
    #[cfg(feature = "wake-onnx")]
    {
        match super::wake_onnx::HeySudo::from_config(cfg) {
            Ok(d) => {
                tracing::info!("hey-sudo wake word active");
                return Box::new(d);
            }
            Err(e) => {
                // Not fatal, and not even a warning unless nothing else takes over:
                // a box may legitimately be configured for Porcupine instead.
                tracing::info!(error = %e, "hey-sudo model unavailable; trying other engines");
            }
        }
    }

    #[cfg(feature = "wake-porcupine")]
    {
        match porcupine::Porcupine::from_config(cfg) {
            Ok(p) => {
                tracing::info!(keyword = %cfg.keyword, "porcupine wake word active");
                return Box::new(p);
            }
            Err(e) => {
                tracing::error!(
                    error = ?e,
                    "could not initialize a wake word engine; wake word is DISABLED. \
                     Text, /listen and the WebSocket gateway still work."
                );
            }
        }
    }

    #[cfg(not(feature = "wake-porcupine"))]
    {
        tracing::warn!("built without the wake-porcupine feature; wake word disabled");
    }

    Box::new(NullWake::new())
}

// ---------------------------------------------------------------------------
// Porcupine C ABI binding
// ---------------------------------------------------------------------------

#[cfg(feature = "wake-porcupine")]
pub mod porcupine {
    use super::*;
    use libloading::{Library, Symbol};
    use std::ffi::{CStr, CString, c_char, c_float, c_int, c_void};
    use std::path::Path;

    type PvStatus = c_int;
    const PV_STATUS_SUCCESS: PvStatus = 0;

    type FnSampleRate = unsafe extern "C" fn() -> i32;
    type FnFrameLength = unsafe extern "C" fn() -> i32;
    type FnInit = unsafe extern "C" fn(
        access_key: *const c_char,
        model_path: *const c_char,
        num_keywords: i32,
        keyword_paths: *const *const c_char,
        sensitivities: *const c_float,
        object: *mut *mut c_void,
    ) -> PvStatus;
    type FnProcess = unsafe extern "C" fn(
        object: *mut c_void,
        pcm: *const i16,
        keyword_index: *mut i32,
    ) -> PvStatus;
    type FnDelete = unsafe extern "C" fn(object: *mut c_void);
    type FnStatusToString = unsafe extern "C" fn(status: PvStatus) -> *const c_char;

    pub struct Porcupine {
        // Field order matters: `handle` is freed in Drop using symbols from `lib`,
        // so the library must outlive it. Rust drops fields in declaration order.
        handle: Handle,
        _lib: Box<Library>,
        frame_length: usize,
    }

    /// Owns the raw object pointer plus the delete/process symbols it needs.
    struct Handle {
        ptr: *mut c_void,
        process: FnProcess,
        delete: FnDelete,
    }

    // The Porcupine object is not shared between threads by this code: it lives on
    // the single capture thread. Send is required to move it there.
    unsafe impl Send for Handle {}

    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.ptr.is_null() {
                unsafe { (self.delete)(self.ptr) };
                self.ptr = std::ptr::null_mut();
            }
        }
    }

    /// Where to look for the shared library, in order. The env var wins so an
    /// operator can point at an unusual location without editing anything.
    fn library_candidates() -> Vec<String> {
        let mut v = Vec::new();
        if let Ok(p) = std::env::var("PV_PORCUPINE_LIB") {
            v.push(p);
        }
        v.extend(
            [
                "/opt/hermit/lib/libpv_porcupine.so",
                "/usr/local/lib/libpv_porcupine.so",
                "/usr/lib/libpv_porcupine.so",
                "libpv_porcupine.so",
            ]
            .map(String::from),
        );
        v
    }

    fn model_path() -> Option<String> {
        std::env::var("PV_PORCUPINE_MODEL").ok().or_else(|| {
            let p = "/opt/hermit/lib/porcupine_params.pv";
            Path::new(p).exists().then(|| p.to_string())
        })
    }

    impl Porcupine {
        pub fn from_config(cfg: &crate::config::Wake) -> Result<Self> {
            let access_key = crate::http::secret("PICOVOICE_ACCESS_KEY")?;

            let keyword_path = match &cfg.keyword_path {
                Some(p) => p.to_string_lossy().to_string(),
                None => {
                    // Built-in keywords ship as .ppn files next to the library.
                    let guess =
                        format!("/opt/hermit/lib/keywords/{}_raspberry-pi.ppn", cfg.keyword);
                    if !Path::new(&guess).exists() {
                        bail!(
                            "no keyword file for {:?}. Copy the .ppn to {guess}, or set \
                             wake.keyword_path in hermit.toml",
                            cfg.keyword
                        );
                    }
                    guess
                }
            };
            let model = model_path().context(
                "porcupine_params.pv not found; copy it to /opt/hermit/lib/ or set \
                 PV_PORCUPINE_MODEL",
            )?;

            Self::open(&access_key, &model, &keyword_path, cfg.sensitivity)
        }

        fn open(
            access_key: &str,
            model_path: &str,
            keyword_path: &str,
            sensitivity: f32,
        ) -> Result<Self> {
            let mut last_err = None;
            for candidate in library_candidates() {
                match unsafe { Library::new(&candidate) } {
                    Ok(lib) => {
                        tracing::debug!(path = %candidate, "loaded libpv_porcupine");
                        return Self::init_with(
                            Box::new(lib),
                            access_key,
                            model_path,
                            keyword_path,
                            sensitivity,
                        );
                    }
                    Err(e) => last_err = Some(format!("{candidate}: {e}")),
                }
            }
            bail!(
                "could not load libpv_porcupine.so (tried {}). Last error: {}",
                library_candidates().join(", "),
                last_err.unwrap_or_default()
            )
        }

        fn init_with(
            lib: Box<Library>,
            access_key: &str,
            model_path: &str,
            keyword_path: &str,
            sensitivity: f32,
        ) -> Result<Self> {
            unsafe {
                let sample_rate: Symbol<FnSampleRate> = lib
                    .get(b"pv_sample_rate\0")
                    .context("libpv_porcupine is missing pv_sample_rate")?;
                let frame_length: Symbol<FnFrameLength> =
                    lib.get(b"pv_porcupine_frame_length\0")
                        .context("libpv_porcupine is missing pv_porcupine_frame_length")?;
                let init: Symbol<FnInit> = lib
                    .get(b"pv_porcupine_init\0")
                    .context("libpv_porcupine is missing pv_porcupine_init")?;
                let process: Symbol<FnProcess> = lib.get(b"pv_porcupine_process\0")?;
                let delete: Symbol<FnDelete> = lib.get(b"pv_porcupine_delete\0")?;
                let status_str: Option<Symbol<FnStatusToString>> =
                    lib.get(b"pv_status_to_string\0").ok();

                let rate = sample_rate() as u32;
                if rate != EXPECTED_SAMPLE_RATE {
                    bail!(
                        "porcupine wants {rate} Hz but the pipeline is {EXPECTED_SAMPLE_RATE} Hz"
                    );
                }
                let frame_len = frame_length() as usize;
                if frame_len == 0 {
                    bail!("porcupine reported a zero frame length");
                }

                let c_access = CString::new(access_key)?;
                let c_model = CString::new(model_path)?;
                let c_keyword = CString::new(keyword_path)?;
                let keyword_paths = [c_keyword.as_ptr()];
                let sensitivities = [sensitivity.clamp(0.0, 1.0) as c_float];

                let mut object: *mut c_void = std::ptr::null_mut();
                let status = init(
                    c_access.as_ptr(),
                    c_model.as_ptr(),
                    1,
                    keyword_paths.as_ptr(),
                    sensitivities.as_ptr(),
                    &mut object,
                );

                if status != PV_STATUS_SUCCESS || object.is_null() {
                    let detail = status_str
                        .map(|f| {
                            let p = f(status);
                            if p.is_null() {
                                format!("status {status}")
                            } else {
                                CStr::from_ptr(p).to_string_lossy().to_string()
                            }
                        })
                        .unwrap_or_else(|| format!("status {status}"));
                    bail!("pv_porcupine_init failed: {detail}");
                }

                // Copy the fn pointers out of the Symbol borrows before moving `lib`.
                let process_fn: FnProcess = *process;
                let delete_fn: FnDelete = *delete;

                Ok(Self {
                    handle: Handle {
                        ptr: object,
                        process: process_fn,
                        delete: delete_fn,
                    },
                    _lib: lib,
                    frame_length: frame_len,
                })
            }
        }
    }

    impl WakeDetector for Porcupine {
        fn frame_length(&self) -> usize {
            self.frame_length
        }

        fn process(&mut self, frame: &[i16]) -> Option<usize> {
            // The C function reads exactly frame_length samples; a short slice
            // would read out of bounds.
            if frame.len() != self.frame_length {
                tracing::warn!(
                    got = frame.len(),
                    want = self.frame_length,
                    "wrong frame size handed to porcupine; ignoring"
                );
                return None;
            }
            let mut index: i32 = -1;
            let status =
                unsafe { (self.handle.process)(self.handle.ptr, frame.as_ptr(), &mut index) };
            if status != PV_STATUS_SUCCESS {
                tracing::warn!(status, "pv_porcupine_process failed");
                return None;
            }
            (index >= 0).then_some(index as usize)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Detector that fires on the Nth frame it sees.
    struct FiresOnFrame {
        n: usize,
        seen: usize,
        frame_length: usize,
    }

    impl WakeDetector for FiresOnFrame {
        fn frame_length(&self) -> usize {
            self.frame_length
        }
        fn process(&mut self, frame: &[i16]) -> Option<usize> {
            assert_eq!(
                frame.len(),
                self.frame_length,
                "frames must be exactly sized"
            );
            self.seen += 1;
            (self.seen == self.n).then_some(0)
        }
    }

    #[test]
    fn feeder_slices_into_exact_frames_across_ragged_reads() {
        let mut f = FrameFeeder::new(FiresOnFrame {
            n: 3,
            seen: 0,
            frame_length: 512,
        });
        // ALSA-shaped reads that do not divide evenly into 512.
        assert_eq!(f.push(&vec![0i16; 320]), None);
        assert_eq!(f.push(&vec![0i16; 320]), None); // 640 -> one frame, 128 left
        assert_eq!(f.push(&vec![0i16; 320]), None); // 448
        assert_eq!(f.push(&vec![0i16; 320]), None); // 768 -> second frame
        // Next full frame is the third -> fires.
        assert_eq!(f.push(&vec![0i16; 512]), Some(0));
    }

    #[test]
    fn feeder_retains_the_remainder_between_pushes() {
        let mut f = FrameFeeder::new(NullWake::new());
        f.push(&[0i16; 100]);
        assert_eq!(f.buffered(), 100);
        f.push(&vec![0i16; 500]);
        // 600 in, one 512 frame consumed, 88 retained.
        assert_eq!(f.buffered(), 88);
    }

    #[test]
    fn feeder_reset_drops_buffered_audio() {
        let mut f = FrameFeeder::new(NullWake::new());
        f.push(&vec![0i16; 300]);
        f.reset();
        assert_eq!(f.buffered(), 0);
    }

    #[test]
    fn null_detector_never_fires() {
        let mut f = FrameFeeder::new(NullWake::new());
        for _ in 0..100 {
            assert_eq!(f.push(&vec![1234i16; 512]), None);
        }
    }

    #[test]
    fn disabled_config_yields_a_silent_detector() {
        let cfg = crate::config::Wake {
            enabled: false,
            ..Default::default()
        };
        let mut d = build(&cfg);
        assert_eq!(d.process(&vec![0i16; d.frame_length()]), None);
    }

    #[test]
    fn build_never_panics_without_the_native_library() {
        // The whole point of dlopen: a missing .so must not stop the daemon.
        let cfg = crate::config::Wake::default();
        let d = build(&cfg);
        assert!(d.frame_length() > 0);
    }
}
