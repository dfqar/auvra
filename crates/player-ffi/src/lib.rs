//! ABI v2: asynchronous, length-delimited UTF-8 JSON control plane.
use player_core::{CommandRequest, PlaybackCoordinator, PlayerEvent, PlayerRuntime, SubmitError};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    ptr, slice,
};
pub const PLAYER_ABI_VERSION: u32 = 2;
const MAX_COMMAND_BYTES: usize = 1024 * 1024;
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerResult {
    Ok = 0,
    InvalidArgument = 1,
    BufferTooSmall = 2,
    Empty = 3,
    Unavailable = 4,
    Panic = 5,
    Busy = 6,
}
struct PendingEvent {
    event: PlayerEvent,
    bytes: Vec<u8>,
}
pub struct PlayerHandle {
    runtime: PlayerRuntime,
    pending: Option<PendingEvent>,
}
fn contained(action: impl FnOnce() -> PlayerResult) -> PlayerResult {
    catch_unwind(AssertUnwindSafe(action)).unwrap_or(PlayerResult::Panic)
}
#[unsafe(no_mangle)]
pub extern "C" fn player_abi_version() -> u32 {
    PLAYER_ABI_VERSION
}
/// # Safety
/// `out` must be writable. A returned handle has one serial caller and must be destroyed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn player_create(out: *mut *mut PlayerHandle) -> PlayerResult {
    if out.is_null() {
        return PlayerResult::InvalidArgument;
    }
    unsafe {
        *out = ptr::null_mut();
    }
    contained(|| {
        let mut core = PlaybackCoordinator::default();
        let Ok(library) = player_library::LibraryWorker::new() else {
            return PlayerResult::Unavailable;
        };
        core.register_library(library);
        let Ok(session) = player_session::SessionWorker::new() else {
            return PlayerResult::Unavailable;
        };
        core.register_session_store(session);
        #[cfg(target_os = "macos")]
        if let Ok(analysis) = audio_engine::OwnedOutputAnalysis::new() {
            core.register_host_analysis(analysis);
        }
        core.register(provider_mock::MockProvider::default());
        core.register(provider_local::LocalPlaybackProvider::default());
        core.register_catalog(provider_local::LocalCatalog);
        let Ok(spotify) = provider_spotify::SpotifyRemoteProvider::new() else {
            return PlayerResult::Unavailable;
        };
        core.register_remote(spotify);
        core.publish(PlayerEvent::Catalog {
            items: provider_mock::demo_tracks(),
        });
        let Ok(runtime) = PlayerRuntime::start(core) else {
            return PlayerResult::Unavailable;
        };
        unsafe {
            *out = Box::into_raw(Box::new(PlayerHandle {
                runtime,
                pending: None,
            }));
        }
        PlayerResult::Ok
    })
}
/// # Safety
/// Null or a live unique handle. No calls may race or follow destruction.
/// Stops and joins the coordinator worker; host calls this on its control queue.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn player_destroy(handle: *mut PlayerHandle) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !handle.is_null() {
            drop(unsafe { Box::from_raw(handle) });
        }
    }));
}
/// # Safety
/// Live serially-owned handle; `bytes` points to `length` readable bytes.
/// Ok means accepted, not executed. Inspect command_result with the same request_id.
/// Busy/InvalidArgument/Unavailable reject without executing or producing a result.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn player_send_json(
    handle: *mut PlayerHandle,
    bytes: *const u8,
    length: usize,
) -> PlayerResult {
    if handle.is_null() || bytes.is_null() || length == 0 || length > MAX_COMMAND_BYTES {
        return PlayerResult::InvalidArgument;
    }
    contained(|| {
        let handle = unsafe { &mut *handle };
        let Ok(request) = serde_json::from_slice::<CommandRequest>(unsafe {
            slice::from_raw_parts(bytes, length)
        }) else {
            return PlayerResult::InvalidArgument;
        };
        match handle.runtime.submit(request) {
            Ok(()) => PlayerResult::Ok,
            Err(SubmitError::InvalidRequest) => PlayerResult::InvalidArgument,
            Err(SubmitError::Busy) => PlayerResult::Busy,
            Err(SubmitError::Unavailable) => PlayerResult::Unavailable,
        }
    })
}
/// # Safety
/// `out_length` is writable; non-null `buffer` has `capacity` writable bytes.
/// Null/short buffer pins this exact event and reports its size without consuming it.
/// A successful copy consumes it and releases command-result credit. JSON has no trailing NUL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn player_event_drain(
    handle: *mut PlayerHandle,
    buffer: *mut u8,
    capacity: usize,
    out_length: *mut usize,
) -> PlayerResult {
    if handle.is_null() || out_length.is_null() {
        return PlayerResult::InvalidArgument;
    }
    unsafe {
        *out_length = 0;
    }
    contained(|| {
        let handle = unsafe { &mut *handle };
        if handle.pending.is_none() {
            let Some(event) = handle.runtime.take_event() else {
                return PlayerResult::Empty;
            };
            let Ok(bytes) = serde_json::to_vec(&event) else {
                handle.runtime.acknowledge(&event);
                return PlayerResult::Unavailable;
            };
            handle.pending = Some(PendingEvent { event, bytes });
        }
        let pending = handle.pending.as_ref().unwrap();
        unsafe {
            *out_length = pending.bytes.len();
        }
        if buffer.is_null() || capacity < pending.bytes.len() {
            return PlayerResult::BufferTooSmall;
        }
        unsafe {
            ptr::copy_nonoverlapping(pending.bytes.as_ptr(), buffer, pending.bytes.len());
        }
        let pending = handle.pending.take().unwrap();
        handle.runtime.acknowledge(&pending.event);
        PlayerResult::Ok
    })
}

/// Pure lyrics normalization, independent of engine handles. Call only on a host IO worker.
/// Input is {format,text,duration_ms}; output is bounded UTF-8 JSON, no trailing NUL.
/// Null/short output returns the required size. Repeating the same input is deterministic.
/// # Safety
/// `bytes` has `length` readable bytes, `out_length` is writable, and non-null
/// `buffer` has `capacity` writable bytes. Input and output must not overlap.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn player_lyrics_parse(
    bytes: *const u8,
    length: usize,
    buffer: *mut u8,
    capacity: usize,
    out_length: *mut usize,
) -> PlayerResult {
    if out_length.is_null() {
        return PlayerResult::InvalidArgument;
    }
    unsafe {
        *out_length = 0;
    }
    if bytes.is_null() || length == 0 || length > MAX_COMMAND_BYTES {
        return PlayerResult::InvalidArgument;
    }
    contained(|| {
        let Ok(output) = player_lyrics::parse_json(unsafe { slice::from_raw_parts(bytes, length) })
        else {
            return PlayerResult::InvalidArgument;
        };
        if output.len() > 2 * MAX_COMMAND_BYTES {
            return PlayerResult::InvalidArgument;
        }
        unsafe {
            *out_length = output.len();
        }
        if buffer.is_null() || capacity < output.len() {
            return PlayerResult::BufferTooSmall;
        }
        unsafe {
            ptr::copy_nonoverlapping(output.as_ptr(), buffer, output.len());
        }
        PlayerResult::Ok
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use player_core::PlayerCommand;
    use player_domain::PlaybackContext;
    use std::{
        thread,
        time::{Duration, Instant},
    };

    #[test]
    fn lyrics_helper_is_bounded_deterministic_and_independent_of_handles() {
        let input = br#"{"format":"lrc","text":"[00:01]An original line","duration_ms":9000}"#;
        let mut size = 999;
        assert_eq!(
            unsafe {
                player_lyrics_parse(input.as_ptr(), input.len(), ptr::null_mut(), 0, &mut size)
            },
            PlayerResult::BufferTooSmall
        );
        let mut output = vec![0u8; size];
        assert_eq!(
            unsafe {
                player_lyrics_parse(
                    input.as_ptr(),
                    input.len(),
                    output.as_mut_ptr(),
                    size,
                    &mut size,
                )
            },
            PlayerResult::Ok
        );
        let first = output.clone();
        assert_eq!(
            unsafe {
                player_lyrics_parse(
                    input.as_ptr(),
                    input.len(),
                    output.as_mut_ptr(),
                    size,
                    &mut size,
                )
            },
            PlayerResult::Ok
        );
        assert_eq!(first, output);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output).unwrap()["precision"],
            "line"
        );
        assert_eq!(
            unsafe { player_lyrics_parse(ptr::null(), 1, ptr::null_mut(), 0, &mut size) },
            PlayerResult::InvalidArgument
        );
        assert_eq!(size, 0);
        assert_eq!(
            unsafe {
                player_lyrics_parse(
                    input.as_ptr(),
                    MAX_COMMAND_BYTES + 1,
                    ptr::null_mut(),
                    0,
                    &mut size,
                )
            },
            PlayerResult::InvalidArgument
        );
    }
    struct Handle(*mut PlayerHandle);
    impl Handle {
        fn new() -> Self {
            let mut handle = ptr::null_mut();
            assert_eq!(unsafe { player_create(&mut handle) }, PlayerResult::Ok);
            Self(handle)
        }
        fn send(&self, id: u64, command: PlayerCommand) -> PlayerResult {
            let bytes = serde_json::to_vec(&CommandRequest {
                request_id: id,
                command,
            })
            .unwrap();
            unsafe { player_send_json(self.0, bytes.as_ptr(), bytes.len()) }
        }
        fn event(&self) -> serde_json::Value {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut length = 0;
            loop {
                let status = unsafe { player_event_drain(self.0, ptr::null_mut(), 0, &mut length) };
                if status == PlayerResult::BufferTooSmall {
                    break;
                }
                assert_eq!(status, PlayerResult::Empty);
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(5));
            }
            let mut bytes = vec![0; length];
            assert_eq!(
                unsafe { player_event_drain(self.0, bytes.as_mut_ptr(), bytes.len(), &mut length) },
                PlayerResult::Ok
            );
            serde_json::from_slice(&bytes).unwrap()
        }
        fn until(&self, kind: &str) -> serde_json::Value {
            loop {
                let event = self.event();
                if event["type"] == kind {
                    return event;
                }
            }
        }
    }
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                player_destroy(self.0);
            }
        }
    }
    #[test]
    fn short_buffers_pin_payload_while_worker_changes_state() {
        let h = Handle::new();
        h.until("catalog");
        h.until("snapshot");
        assert_eq!(
            h.send(
                1,
                PlayerCommand::ReplaceQueue {
                    items: provider_mock::demo_tracks(),
                    start: 0,
                    context: PlaybackContext::UserQueue
                }
            ),
            PlayerResult::Ok
        );
        h.until("command_result");
        let mut length = 0;
        while unsafe { player_event_drain(h.0, ptr::null_mut(), 0, &mut length) }
            == PlayerResult::Empty
        {
            thread::sleep(Duration::from_millis(5));
        }
        let original = length;
        thread::sleep(Duration::from_millis(250));
        let mut tiny = [0];
        assert_eq!(
            unsafe { player_event_drain(h.0, tiny.as_mut_ptr(), 1, &mut length) },
            PlayerResult::BufferTooSmall
        );
        assert_eq!(length, original);
        let mut bytes = vec![0; length];
        assert_eq!(
            unsafe { player_event_drain(h.0, bytes.as_mut_ptr(), bytes.len(), &mut length) },
            PlayerResult::Ok
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["type"],
            "snapshot"
        );
    }
    #[test]
    fn accepted_domain_error_is_correlated_and_does_not_replace_queue() {
        let h = Handle::new();
        h.until("catalog");
        h.until("snapshot");
        let mut local = provider_mock::demo_tracks()[0].clone();
        local.id = player_domain::MediaId::parse("spotify:track:pending").unwrap();
        assert_eq!(
            h.send(
                9,
                PlayerCommand::ReplaceQueue {
                    items: vec![local],
                    start: 0,
                    context: PlaybackContext::UserQueue
                }
            ),
            PlayerResult::Ok
        );
        let result = h.until("command_result");
        assert_eq!(result["request_id"], 9);
        assert_eq!(result["error"]["code"], "provider_unavailable");
        let state = h.until("snapshot");
        assert!(state["state"]["queue"]["current"].is_null());
    }
    #[test]
    fn rejected_input_and_result_backpressure_do_not_execute() {
        let h = Handle::new();
        unsafe {
            assert_eq!(
                player_create(ptr::null_mut()),
                PlayerResult::InvalidArgument
            );
            for bytes in [
                &b"{}"[..],
                &b"\xff"[..],
                &b"{\"request_id\":0,\"command\":{\"type\":\"stop\"}}"[..],
            ] {
                assert_eq!(
                    player_send_json(h.0, bytes.as_ptr(), bytes.len()),
                    PlayerResult::InvalidArgument
                );
            }
            assert_eq!(
                player_send_json(h.0, ptr::null(), 1),
                PlayerResult::InvalidArgument
            );
            assert_eq!(
                player_send_json(h.0, b"x".as_ptr(), MAX_COMMAND_BYTES + 1),
                PlayerResult::InvalidArgument
            );
            player_destroy(ptr::null_mut());
        }
        for id in 1..=64 {
            assert_eq!(h.send(id, PlayerCommand::Stop), PlayerResult::Ok);
        }
        assert_eq!(
            h.send(65, PlayerCommand::SetVolume { volume: 0.1 }),
            PlayerResult::Busy
        );
        let result = h.until("command_result");
        assert_eq!(result["request_id"], 1);
        assert_eq!(
            h.send(65, PlayerCommand::SetVolume { volume: 0.1 }),
            PlayerResult::Ok
        );
    }
    #[test]
    fn independent_handles_have_independent_state() {
        let first = Handle::new();
        let second = Handle::new();
        first.send(
            1,
            PlayerCommand::ReplaceQueue {
                items: provider_mock::demo_tracks(),
                start: 1,
                context: PlaybackContext::UserQueue,
            },
        );
        let idle = second.until("snapshot");
        assert_eq!(idle["state"]["state"], "idle");
        first.until("command_result");
        let playing = first.until("snapshot");
        assert_eq!(playing["state"]["state"], "playing");
    }
}
