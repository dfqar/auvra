#![allow(clippy::missing_safety_doc)] // Safety contracts are expressed by the generated C header and native RAII wrapper.

use std::{
    ffi::{CStr, CString, c_char, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    ptr, slice,
    time::Instant,
};
use still_core::{
    AmbientFrame, AmbientSmoothing, AutoQualityPolicy, FrameClock, InputMailbox, PointerInput,
    PowerState, Quality, SceneModulation, SceneParameters, SignalBus,
};
use still_renderer::{Renderer, RendererError};
use still_scenes::{FROST_DESCRIPTOR, SCENE_DESCRIPTORS, Scene, create_scene};

mod metrics;
use metrics::Metrics;

pub const STILL_ABI_VERSION: u32 = 2;

#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StillResult {
    Ok = 0,
    InvalidArgument = 1,
    InvalidState = 2,
    Renderer = 3,
    BufferTooSmall = 4,
    Panic = 5,
    NotFound = 6,
}

#[repr(C)]
pub struct StillEngineConfig {
    pub abi_version: u32,
    pub struct_size: u32,
    pub quality: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct StillPointerEvent {
    pub abi_version: u32,
    pub struct_size: u32,
    pub x: f32,
    pub y: f32,
    pub velocity_x: f32,
    pub velocity_y: f32,
    pub active: u32,
    pub reserved: u32,
}

/// Latest media palette, in normalized sRGB. This leaves the manual light parameters unchanged.
/// Each color is RGB; energy is a normalized analyzed envelope, not audio PCM.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct StillAmbientFrame {
    pub abi_version: u32,
    pub struct_size: u32,
    pub color1: [f32; 3],
    pub color2: [f32; 3],
    pub color3: [f32; 3],
    pub energy: f32,
}

/// Code-only timing in seconds. Zero disables smoothing for that property.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct StillAmbientSmoothing {
    pub abi_version: u32,
    pub struct_size: u32,
    pub color_seconds: f32,
    pub attack_seconds: f32,
    pub release_seconds: f32,
    pub transform_seconds: f32,
}

/// Generic transient scene overlay; the caller owns its time base and origin.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct StillModulationFrame {
    pub abi_version: u32,
    pub struct_size: u32,
    pub time_seconds: f64,
    pub motion: f32,
    pub turbulence: f32,
    pub light: f32,
    pub reserved: u32,
}

pub struct StillEngine {
    renderer: Renderer,
    scene: Option<Box<dyn Scene>>,
    clock: FrameClock,
    parameters: SceneParameters,
    input_mailbox: InputMailbox,
    signals: SignalBus,
    quality: AutoQualityPolicy,
    suspended: bool,
    last_error: Option<CString>,
    metrics: Metrics,
    last_present_timestamp: Option<f64>,
    ambient_input: Option<AmbientFrame>,
    ambient_smoothing: AmbientSmoothing,
    modulation: Option<SceneModulation>,
    last_presented_modulation: Option<SceneModulation>,
}

impl StillEngine {
    fn build_scene(&self, id: &str, quality: Quality, seed: u64) -> Result<Box<dyn Scene>, String> {
        use still_renderer::wgpu::ErrorFilter;
        self.renderer
            .device
            .push_error_scope(ErrorFilter::OutOfMemory);
        self.renderer
            .device
            .push_error_scope(ErrorFilter::Validation);
        let scene = catch_unwind(AssertUnwindSafe(|| {
            create_scene(
                id,
                &self.renderer.device,
                &self.renderer.adapter,
                quality,
                seed,
            )
        }));
        let validation = pollster::block_on(self.renderer.device.pop_error_scope());
        let memory = pollster::block_on(self.renderer.device.pop_error_scope());
        if let Some(error) = validation.or(memory) {
            return Err(format!("Cannot initialize {id}: {error}"));
        }
        let mut scene = scene
            .map_err(|_| format!("Cannot initialize {id}: GPU pipeline creation failed"))?
            .ok_or_else(|| format!("Unknown scene {id}"))?;
        scene.set_ambient_input(self.ambient_input);
        scene.set_ambient_smoothing(self.ambient_smoothing);
        scene.set_modulation(self.modulation);
        Ok(scene)
    }

    fn create(quality: Quality) -> Result<Self, String> {
        Ok(Self {
            renderer: Renderer::new().map_err(|e| e.to_string())?,
            scene: None,
            clock: FrameClock::default(),
            parameters: SceneParameters::defaults(&FROST_DESCRIPTOR),
            input_mailbox: InputMailbox::default(),
            signals: SignalBus::default(),
            quality: AutoQualityPolicy::new(quality),
            suspended: false,
            last_error: None,
            metrics: Metrics::default(),
            last_present_timestamp: None,
            ambient_input: None,
            ambient_smoothing: AmbientSmoothing::default(),
            modulation: None,
            last_presented_modulation: None,
        })
    }

    fn fail(&mut self, result: StillResult, message: impl ToString) -> StillResult {
        let clean = message.to_string().replace('\0', " ");
        self.last_error = CString::new(clean).ok();
        result
    }

    fn apply_frame_boundary_updates(&mut self, dt: f32) {
        self.signals.advance(dt, self.input_mailbox.take());
    }

    fn set_ambient_input(&mut self, input: Option<AmbientFrame>) {
        self.ambient_input = input;
        if let Some(scene) = &mut self.scene {
            scene.set_ambient_input(input);
        }
    }

    fn set_ambient_smoothing(&mut self, settings: AmbientSmoothing) {
        self.ambient_smoothing = settings;
        if let Some(scene) = &mut self.scene {
            scene.set_ambient_smoothing(settings);
        }
    }

    fn set_modulation(&mut self, input: Option<SceneModulation>) {
        self.modulation = input;
        if let Some(scene) = &mut self.scene {
            scene.set_modulation(input);
        }
    }

    fn export_parameters(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(&self.parameters.to_persisted(&FROST_DESCRIPTOR))
    }

    fn select_scene(&self, stable_id: &str) -> StillResult {
        if stable_id == FROST_DESCRIPTOR.stable_id {
            StillResult::Ok
        } else {
            StillResult::NotFound
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn still_abi_version() -> u32 {
    STILL_ABI_VERSION
}

/// Replace a transient visual overlay on the engine's serial owner. Null clears
/// immediately; invalid input is rejected atomically. Saved parameters are untouched.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn still_engine_modulation_update(
    engine: *mut StillEngine,
    frame: *const StillModulationFrame,
) -> StillResult {
    with_engine(engine, |e| {
        if frame.is_null() {
            e.set_modulation(None);
            return StillResult::Ok;
        }
        let frame = unsafe { &*frame };
        if frame.abi_version != STILL_ABI_VERSION
            || frame.struct_size < std::mem::size_of::<StillModulationFrame>() as u32
        {
            return StillResult::InvalidArgument;
        }
        let Some(input) = (SceneModulation {
            time_seconds: frame.time_seconds,
            motion: frame.motion,
            turbulence: frame.turbulence,
            light: frame.light,
        })
        .validated() else {
            return StillResult::InvalidArgument;
        };
        e.set_modulation(Some(input));
        StillResult::Ok
    })
}

/// Atomically replace the transient palette/envelope. A null frame clears it
/// and fades back to manual colors. Call on the same serial queue as the engine.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn still_engine_ambient_update(
    engine: *mut StillEngine,
    frame: *const StillAmbientFrame,
) -> StillResult {
    with_engine(engine, |e| {
        if frame.is_null() {
            e.set_ambient_input(None);
            return StillResult::Ok;
        }
        let frame = unsafe { &*frame };
        if frame.abi_version != STILL_ABI_VERSION
            || frame.struct_size < std::mem::size_of::<StillAmbientFrame>() as u32
        {
            return StillResult::InvalidArgument;
        }
        let Some(input) =
            AmbientFrame::validated([frame.color1, frame.color2, frame.color3], frame.energy)
        else {
            return StillResult::InvalidArgument;
        };
        e.set_ambient_input(Some(input));
        StillResult::Ok
    })
}

/// Set external response times without changing manual scene controls or state.
/// Null resets the application-friendly defaults. Invalid settings are atomic.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn still_engine_ambient_smoothing_set(
    engine: *mut StillEngine,
    settings: *const StillAmbientSmoothing,
) -> StillResult {
    with_engine(engine, |e| {
        if settings.is_null() {
            e.set_ambient_smoothing(AmbientSmoothing::default());
            return StillResult::Ok;
        }
        let settings = unsafe { &*settings };
        if settings.abi_version != STILL_ABI_VERSION
            || settings.struct_size < std::mem::size_of::<StillAmbientSmoothing>() as u32
        {
            return StillResult::InvalidArgument;
        }
        let settings = AmbientSmoothing {
            color_seconds: settings.color_seconds,
            attack_seconds: settings.attack_seconds,
            release_seconds: settings.release_seconds,
            transform_seconds: settings.transform_seconds,
        };
        let Some(settings) = settings.validated() else {
            return StillResult::InvalidArgument;
        };
        e.set_ambient_smoothing(settings);
        StillResult::Ok
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn still_engine_create(
    config: *const StillEngineConfig,
    out_engine: *mut *mut StillEngine,
) -> StillResult {
    ffi(|| {
        if config.is_null() || out_engine.is_null() {
            return StillResult::InvalidArgument;
        }
        unsafe {
            *out_engine = ptr::null_mut();
        }
        let config = unsafe { &*config };
        if config.abi_version != STILL_ABI_VERSION
            || config.struct_size < std::mem::size_of::<StillEngineConfig>() as u32
        {
            return StillResult::InvalidArgument;
        }
        match StillEngine::create(Quality::from_raw(config.quality)) {
            Ok(engine) => {
                unsafe {
                    *out_engine = Box::into_raw(Box::new(engine));
                }
                StillResult::Ok
            }
            Err(error) => {
                eprintln!("Still renderer initialization failed: {error}");
                StillResult::Renderer
            }
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn still_engine_destroy(engine: *mut StillEngine) {
    if !engine.is_null() {
        let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
            drop(Box::from_raw(engine));
        }));
    }
}

/// Attach a retained AppKit NSView with a CAMetalLayer on macOS.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn still_engine_surface_attach(
    engine: *mut StillEngine,
    ns_view: *mut c_void,
    width: u32,
    height: u32,
) -> StillResult {
    with_engine(engine, |e| {
        if let Err(err) = unsafe { e.renderer.attach(ns_view, width, height) } {
            return e.fail(StillResult::Renderer, err);
        }
        let mut scene = match e.build_scene(
            FROST_DESCRIPTOR.stable_id,
            e.quality.resolved(),
            still_scenes::FROST_SEED,
        ) {
            Ok(scene) => scene,
            Err(error) => {
                e.scene = None;
                e.renderer.detach();
                return e.fail(StillResult::Renderer, error);
            }
        };
        scene.set_parameters(e.parameters.clone());
        e.scene = Some(scene);
        e.clock.reset();

        StillResult::Ok
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn still_engine_surface_detach(engine: *mut StillEngine) -> StillResult {
    with_engine(engine, |e| {
        e.scene = None;
        e.renderer.detach();
        e.clock.reset();

        StillResult::Ok
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn still_engine_resize(
    engine: *mut StillEngine,
    width: u32,
    height: u32,
) -> StillResult {
    with_engine(engine, |e| {
        e.renderer.resize(width, height);
        if (width == 0 || height == 0)
            && let Some(scene) = &mut e.scene
        {
            scene.release_gpu();
        }
        StillResult::Ok
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn still_engine_suspend(engine: *mut StillEngine) -> StillResult {
    with_engine(engine, |e| {
        e.suspended = true;
        e.renderer.set_suspended(true);
        if let Some(scene) = &mut e.scene {
            scene.suspend();
        }
        e.clock.reset();
        e.signals.reset_transients();
        StillResult::Ok
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn still_engine_resume(engine: *mut StillEngine) -> StillResult {
    with_engine(engine, |e| {
        e.suspended = false;
        e.renderer.set_suspended(false);
        if let Some(scene) = &mut e.scene {
            scene.resume();
        }
        e.clock.reset();
        StillResult::Ok
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn still_engine_frame_tick(
    engine: *mut StillEngine,
    timestamp_seconds: f64,
) -> StillResult {
    frame_tick(engine, timestamp_seconds, false)
}

/// Continuous display frames preserve palette interpolation across resume.
/// Use the ordinary frame tick for a single inactive-window paint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn still_engine_frame_tick_animated(
    engine: *mut StillEngine,
    timestamp_seconds: f64,
) -> StillResult {
    frame_tick(engine, timestamp_seconds, true)
}

fn frame_tick(engine: *mut StillEngine, timestamp_seconds: f64, animated: bool) -> StillResult {
    with_engine(engine, |e| {
        if !timestamp_seconds.is_finite() {
            return StillResult::InvalidArgument;
        }
        if e.suspended || !e.renderer.is_renderable() {
            return StillResult::Ok;
        }
        let mut plan = e.clock.advance(timestamp_seconds);
        plan.animated = animated;
        let update_start = Instant::now();
        e.apply_frame_boundary_updates(plan.display_dt);
        let update_cpu = update_start.elapsed().as_secs_f32();
        let frame = match e.renderer.acquire() {
            Ok(frame) => frame,
            Err(RendererError::Suspended | RendererError::Detached) => return StillResult::Ok,
            Err(err @ RendererError::Frame(still_renderer::wgpu::SurfaceError::OutOfMemory)) => {
                e.suspended = true;
                e.renderer.set_suspended(true);
                return e.fail(StillResult::Renderer, err);
            }
            Err(err) => return e.fail(StillResult::Renderer, err),
        };
        let encode_start = Instant::now();
        let Some(scene) = &mut e.scene else {
            return StillResult::InvalidState;
        };
        let viewport = e.renderer.size();
        scene.prepare(&e.renderer.device, viewport);
        let mut encoder = e.renderer.create_encoder();
        let pointer = e.signals.pointer();
        let Some(output) = scene.encode(&e.renderer.queue, &mut encoder, plan, pointer) else {
            return StillResult::Ok;
        };
        if let Err(err) = e
            .renderer
            .prepare_compositor_source(output.source_key, output.view)
        {
            return e.fail(StillResult::Renderer, err);
        }
        if let Err(err) = e.renderer.encode_composite(&mut encoder, &frame.view) {
            return e.fail(StillResult::Renderer, err);
        }
        e.renderer.queue.submit(Some(encoder.finish()));
        frame.texture.present();
        e.last_presented_modulation = if scene.descriptor().supports_modulation {
            e.modulation
        } else {
            None
        };
        let encode_cpu = encode_start.elapsed().as_secs_f32();
        // Cadence uses the uncapped display timestamp, independent of simulation
        // catch-up limits and resets on scene changes. Long gaps stay visible.
        let display_dt =
            e.last_present_timestamp
                .map_or(0.0, |previous| (timestamp_seconds - previous).max(0.0)) as f32;
        e.last_present_timestamp = Some(timestamp_seconds);
        e.metrics.record(update_cpu + encode_cpu, display_dt);
        if e.quality.observe(update_cpu + encode_cpu, display_dt) {
            let resolved = e.quality.resolved();
            if let Some(scene) = &mut e.scene {
                scene.set_quality(resolved);
            }
        }
        StillResult::Ok
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn still_engine_input_batch(
    engine: *mut StillEngine,
    events: *const StillPointerEvent,
    count: usize,
) -> StillResult {
    with_engine(engine, |e| {
        if count == 0 {
            return StillResult::Ok;
        }
        if events.is_null() {
            return StillResult::InvalidArgument;
        }
        for event in unsafe { slice::from_raw_parts(events, count) } {
            if event.abi_version != STILL_ABI_VERSION
                || event.struct_size < std::mem::size_of::<StillPointerEvent>() as u32
            {
                return StillResult::InvalidArgument;
            }
            if [event.x, event.y, event.velocity_x, event.velocity_y]
                .into_iter()
                .all(f32::is_finite)
            {
                e.input_mailbox.push(PointerInput {
                    position: [event.x.clamp(-1.0, 1.0), event.y.clamp(-1.0, 1.0)],
                    velocity: [event.velocity_x, event.velocity_y],
                    active: event.active != 0,
                });
            }
        }
        StillResult::Ok
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn still_engine_parameter_set(
    engine: *mut StillEngine,
    stable_id: *const c_char,
    value: f32,
) -> StillResult {
    with_engine(engine, |e| {
        if !value.is_finite() {
            return StillResult::InvalidArgument;
        }
        let Some(stable_id) = read_string(stable_id) else {
            return StillResult::InvalidArgument;
        };
        let Some(value) = e.parameters.set(&FROST_DESCRIPTOR, stable_id, value) else {
            return StillResult::InvalidArgument;
        };
        if let Some(scene) = &mut e.scene {
            let _ = scene.set_parameter(stable_id, value);
        }
        StillResult::Ok
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn still_engine_parameter_get(
    engine: *mut StillEngine,
    stable_id: *const c_char,
    out_value: *mut f32,
) -> StillResult {
    with_engine(engine, |e| {
        if out_value.is_null() {
            return StillResult::InvalidArgument;
        }
        let Some(stable_id) = read_string(stable_id) else {
            return StillResult::InvalidArgument;
        };
        let Some(value) = e.parameters.get(&FROST_DESCRIPTOR, stable_id) else {
            return StillResult::InvalidArgument;
        };
        unsafe {
            *out_value = value;
        }
        StillResult::Ok
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn still_engine_scene_select(
    engine: *mut StillEngine,
    stable_id: *const c_char,
) -> StillResult {
    with_engine(engine, |e| {
        let Some(stable_id) = read_string(stable_id) else {
            return StillResult::InvalidArgument;
        };
        e.select_scene(stable_id)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn still_engine_scene_catalog_export(
    engine: *mut StillEngine,
    buffer: *mut u8,
    capacity: usize,
    out_length: *mut usize,
) -> StillResult {
    with_engine(engine, |e| {
        let scenes = SCENE_DESCRIPTORS
            .iter()
            .map(|scene| {
                serde_json::json!({
                    "id": scene.stable_id,
                    "name": scene.display_name,
                    "schemaVersion": scene.schema_version,
                    "supportsModulation": scene.supports_modulation,
                })
            })
            .collect::<Vec<_>>();
        let catalog = serde_json::json!({
            "selected": FROST_DESCRIPTOR.stable_id,
            "scenes": scenes,
        });
        match serde_json::to_vec(&catalog) {
            Ok(bytes) => copy_bytes(&bytes, buffer, capacity, out_length),
            Err(error) => e.fail(StillResult::InvalidState, error),
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn still_engine_display_refresh_set(
    engine: *mut StillEngine,
    refresh_hz: f32,
) -> StillResult {
    with_engine(engine, |e| {
        if !refresh_hz.is_finite() {
            return StillResult::InvalidArgument;
        }
        e.quality.set_refresh_hz(refresh_hz);
        StillResult::Ok
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn still_engine_power_state_set(
    engine: *mut StillEngine,
    low_power: u32,
    thermal_level: u32,
) -> StillResult {
    with_engine(engine, |e| {
        let before = e.quality.resolved();
        let power = PowerState {
            low_power: low_power != 0,
            thermal_level: thermal_level.min(3) as u8,
        };
        e.quality.set_power(power);
        if before != e.quality.resolved()
            && let Some(scene) = &mut e.scene
        {
            scene.set_quality(e.quality.resolved());
        }
        StillResult::Ok
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn still_engine_state_export(
    engine: *mut StillEngine,
    buffer: *mut u8,
    capacity: usize,
    out_length: *mut usize,
) -> StillResult {
    with_engine(engine, |e| match e.export_parameters() {
        Ok(bytes) => copy_bytes(&bytes, buffer, capacity, out_length),
        Err(err) => e.fail(StillResult::InvalidState, err),
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn still_engine_metrics_export(
    engine: *mut StillEngine,
    buffer: *mut u8,
    capacity: usize,
    out_length: *mut usize,
) -> StillResult {
    with_engine(engine, |e| {
        let mut metrics = e.metrics.export();
        metrics["active_scene"] = FROST_DESCRIPTOR.stable_id.into();
        metrics["quality_mode"] = (e.quality.selected() as u32).into();
        metrics["quality_tier"] = (e.quality.resolved() as u32).into();
        metrics["viewport_pixels"] = serde_json::json!(e.renderer.size());
        metrics["modulation"] = serde_json::json!(e.modulation);
        metrics["last_presented_modulation"] = serde_json::json!(e.last_presented_modulation);
        match serde_json::to_vec_pretty(&metrics) {
            Ok(bytes) => copy_bytes(&bytes, buffer, capacity, out_length),
            Err(err) => e.fail(StillResult::InvalidState, err),
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn still_engine_error_drain(
    engine: *mut StillEngine,
    buffer: *mut c_char,
    capacity: usize,
    out_length: *mut usize,
) -> StillResult {
    with_engine(engine, |e| {
        drain_message(&mut e.last_error, buffer, capacity, out_length)
    })
}

fn read_string<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(ptr) }.to_str().ok()
    }
}

fn copy_bytes(
    bytes: &[u8],
    buffer: *mut u8,
    capacity: usize,
    out_length: *mut usize,
) -> StillResult {
    if out_length.is_null() {
        return StillResult::InvalidArgument;
    }
    unsafe {
        *out_length = bytes.len();
    }
    if buffer.is_null() || capacity < bytes.len() {
        return StillResult::BufferTooSmall;
    }
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len());
    }
    StillResult::Ok
}

fn drain_message(
    slot: &mut Option<CString>,
    buffer: *mut c_char,
    capacity: usize,
    out_length: *mut usize,
) -> StillResult {
    if out_length.is_null() {
        return StillResult::InvalidArgument;
    }
    let Some(message) = slot.take() else {
        unsafe {
            *out_length = 0;
        }
        return StillResult::Ok;
    };
    let bytes = message.as_bytes_with_nul();
    unsafe {
        *out_length = bytes.len();
    }
    if buffer.is_null() || capacity < bytes.len() {
        *slot = Some(message);
        return StillResult::BufferTooSmall;
    }
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr().cast(), buffer, bytes.len());
    }
    StillResult::Ok
}

fn ffi(f: impl FnOnce() -> StillResult) -> StillResult {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(StillResult::Panic)
}
fn with_engine(
    engine: *mut StillEngine,
    f: impl FnOnce(&mut StillEngine) -> StillResult,
) -> StillResult {
    if engine.is_null() {
        return StillResult::InvalidArgument;
    }
    let engine = unsafe { &mut *engine };
    match catch_unwind(AssertUnwindSafe(|| f(engine))) {
        Ok(result) => result,
        Err(_) => {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                engine.renderer.recover_after_frame_failure()
            }));
            engine.fail(
                StillResult::Panic,
                "Rust panic contained at the C ABI boundary",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modulation_abi_is_atomic_transient_and_independent_of_manual_state() {
        let Ok(mut engine) = StillEngine::create(Quality::Eco) else {
            assert_ne!(
                std::env::var("STILL_REQUIRE_METAL_TESTS").as_deref(),
                Ok("1")
            );
            return;
        };
        let saved = engine.export_parameters().unwrap();
        let mut frame = StillModulationFrame {
            abi_version: STILL_ABI_VERSION,
            struct_size: std::mem::size_of::<StillModulationFrame>() as u32,
            time_seconds: 12.25,
            motion: 0.8,
            turbulence: 0.3,
            light: 0.6,
            reserved: 0,
        };
        unsafe {
            assert_eq!(
                still_engine_modulation_update(&mut engine, &frame),
                StillResult::Ok
            );
            let accepted = engine.modulation;
            frame.motion = f32::NAN;
            assert_eq!(
                still_engine_modulation_update(&mut engine, &frame),
                StillResult::InvalidArgument
            );
            assert_eq!(engine.modulation, accepted);
            frame.motion = 0.5;
            frame.struct_size = 0;
            assert_eq!(
                still_engine_modulation_update(&mut engine, &frame),
                StillResult::InvalidArgument
            );
            assert_eq!(engine.modulation, accepted);
            assert_eq!(engine.export_parameters().unwrap(), saved);
            assert_eq!(
                still_engine_modulation_update(&mut engine, ptr::null()),
                StillResult::Ok
            );
            assert!(engine.modulation.is_none());
        }
        assert_eq!(
            SCENE_DESCRIPTORS
                .iter()
                .filter(|s| s.supports_modulation)
                .map(|s| s.stable_id)
                .collect::<Vec<_>>(),
            vec!["frost"]
        );
    }

    #[test]
    fn ffi_rejects_null_handles_invalid_configs_and_nonfinite_inputs() {
        unsafe {
            assert_eq!(
                still_engine_frame_tick(ptr::null_mut(), 0.0),
                StillResult::InvalidArgument
            );
            assert_eq!(
                still_engine_scene_select(ptr::null_mut(), ptr::null()),
                StillResult::InvalidArgument
            );
            still_engine_destroy(ptr::null_mut());
            let config = StillEngineConfig {
                abi_version: 0,
                struct_size: 0,
                quality: 0,
                reserved: 0,
            };
            let mut handle = ptr::dangling_mut();
            assert_eq!(
                still_engine_create(&config, &mut handle),
                StillResult::InvalidArgument
            );
            assert!(handle.is_null());
            let Ok(mut engine) = StillEngine::create(Quality::Balanced) else {
                return;
            };
            assert_eq!(
                still_engine_frame_tick(&mut engine, f64::NAN),
                StillResult::InvalidArgument
            );
            assert_eq!(
                still_engine_display_refresh_set(&mut engine, f32::INFINITY),
                StillResult::InvalidArgument
            );
            let parameter = c"frost.brightness";
            assert_eq!(
                still_engine_parameter_set(&mut engine, parameter.as_ptr(), f32::NAN),
                StillResult::InvalidArgument
            );
            assert_eq!(
                still_engine_input_batch(&mut engine, ptr::null(), 1),
                StillResult::InvalidArgument
            );
        }
    }

    #[test]
    fn configured_light_parameters_survive_lifecycle_before_surface_attachment() {
        let Ok(mut engine) = StillEngine::create(Quality::Balanced) else {
            return;
        };
        unsafe {
            assert_eq!(
                still_engine_parameter_set(&mut engine, c"frost.light2.red".as_ptr(), 0.43),
                StillResult::Ok
            );
            assert_eq!(still_engine_surface_detach(&mut engine), StillResult::Ok);
            let mut value = 0.0;
            assert_eq!(
                still_engine_parameter_get(&mut engine, c"frost.light2.red".as_ptr(), &mut value),
                StillResult::Ok
            );
            assert_eq!(value, 0.43);
        }
    }

    #[test]
    fn ambient_input_is_transient_atomic_and_can_be_cleared() {
        let Ok(mut engine) = StillEngine::create(Quality::Balanced) else {
            return;
        };
        engine.select_scene("frost");
        let saved = engine.export_parameters().unwrap();
        let mut input = StillAmbientFrame {
            abi_version: STILL_ABI_VERSION,
            struct_size: std::mem::size_of::<StillAmbientFrame>() as u32,
            color1: [1.0, 0.0, 0.2],
            color2: [0.2, 0.6, 1.0],
            color3: [0.6, 1.0, 0.2],
            energy: 0.75,
        };
        unsafe {
            assert_eq!(
                still_engine_ambient_update(&mut engine, &input),
                StillResult::Ok
            );
            let previous = engine.ambient_input;
            input.color2[1] = f32::NAN;
            assert_eq!(
                still_engine_ambient_update(&mut engine, &input),
                StillResult::InvalidArgument
            );
            assert_eq!(engine.ambient_input, previous);
            input.color2[1] = 0.5;
            input.struct_size = 4;
            assert_eq!(
                still_engine_ambient_update(&mut engine, &input),
                StillResult::InvalidArgument
            );
            assert_eq!(engine.ambient_input, previous);
            assert_eq!(engine.export_parameters().unwrap(), saved);
            assert_eq!(
                still_engine_ambient_update(&mut engine, ptr::null()),
                StillResult::Ok
            );
            assert!(engine.ambient_input.is_none());
            assert_eq!(engine.export_parameters().unwrap(), saved);
            assert_eq!(
                still_engine_ambient_update(ptr::null_mut(), &input),
                StillResult::InvalidArgument
            );
        }
    }

    #[test]
    fn smoothing_settings_are_atomic_and_transient() {
        let Ok(mut engine) = StillEngine::create(Quality::Balanced) else {
            return;
        };
        engine.select_scene("frost");
        let saved = engine.export_parameters().unwrap();
        let mut settings = StillAmbientSmoothing {
            abi_version: STILL_ABI_VERSION,
            struct_size: std::mem::size_of::<StillAmbientSmoothing>() as u32,
            color_seconds: 0.8,
            attack_seconds: 0.008,
            release_seconds: 0.2,
            transform_seconds: 0.05,
        };
        unsafe {
            assert_eq!(
                still_engine_ambient_smoothing_set(&mut engine, &settings),
                StillResult::Ok
            );
            let previous = engine.ambient_smoothing;
            settings.attack_seconds = -1.0;
            assert_eq!(
                still_engine_ambient_smoothing_set(&mut engine, &settings),
                StillResult::InvalidArgument
            );
            assert_eq!(engine.ambient_smoothing, previous);
            settings.attack_seconds = 0.01;
            settings.struct_size = 4;
            assert_eq!(
                still_engine_ambient_smoothing_set(&mut engine, &settings),
                StillResult::InvalidArgument
            );
            assert_eq!(
                still_engine_ambient_smoothing_set(&mut engine, ptr::null()),
                StillResult::Ok
            );
            assert_eq!(engine.ambient_smoothing, AmbientSmoothing::default());
            assert_eq!(engine.export_parameters().unwrap(), saved);
            assert_eq!(
                still_engine_ambient_smoothing_set(ptr::null_mut(), ptr::null()),
                StillResult::InvalidArgument
            );
        }
    }
}
