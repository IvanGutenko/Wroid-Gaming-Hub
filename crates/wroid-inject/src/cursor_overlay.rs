//! Wayland cursor overlay for UI-cursor mode on the desktop compositor.
//!
//! Creates a transparent, fullscreen `zwlr_layer_shell_v1` overlay surface on
//! the **desktop** Wayland display (KWin and friends), above the fullscreen
//! Gamescope window. The session-drawn virtual cursor sprite is rendered by
//! the desktop compositor: Gamescope's nested server accepts layer-shell
//! surfaces at the protocol level but never composites them into its output,
//! so a sprite inside Gamescope is invisible no matter how correctly its
//! buffers are committed.
//!
//! The surface is always input-transparent (empty input region, no keyboard
//! interactivity): the session grabs the physical mouse itself and drives the
//! sprite from raw evdev deltas, so the overlay must never intercept desktop
//! input. `wl_pointer` events remain a secondary source and are emitted as
//! [`CursorEvent`]s for compositors that deliver them while the mouse is not
//! grabbed.

use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_output, wl_pointer, wl_region, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

/// Linux input event code for the left mouse button (`BTN_LEFT`).
const BTN_LEFT: u32 = 0x110;
/// Poll timeout (ms) for the Wayland event loop so commands are noticed.
// A short poll keeps queued sprite commands latency low; the thread sleeps
// in the kernel between ticks, so this is not a busy loop.
const POLL_TIMEOUT_MS: i32 = 5;
/// Timeout for the overlay startup handshake.
// The overlay maps during the Waydroid boot storm (LXC start, Android boot,
// package launch, gamescope FSR init), when the desktop compositor can stall
// surface configuration for seconds at a time. A tight deadline here used to
// disable the UI cursor for whole sessions.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// One presented frame, one compositor-retained frame, and one writable frame.
const SPRITE_BUFFER_COUNT: usize = 3;

/// Classic left-leaning arrow cursor sprite, `X` = black outline, `o` = white
/// fill, `.` = transparent. Blitted directly into the overlay surface's
/// full-surface buffers so UI cursor mode keeps a visible pointer even when
/// the host compositor hides its sprite.
const CURSOR_ROWS: &[&str] = &[
    "X...........",
    "XX..........",
    "XoX.........",
    "XooX........",
    "XoooX.......",
    "XooooX......",
    "XoooooX.....",
    "XooooooX....",
    "XoooooooX...",
    "XooooooooX..",
    "XoooooooooX.",
    "XooooXXXXXX.",
    "XooXoX......",
    "XoXX.XoX....",
    "XX...XoX....",
    "X.....XoX...",
    "......XX....",
];
const CURSOR_WIDTH: i32 = 12;
const CURSOR_HEIGHT: i32 = CURSOR_ROWS.len() as i32;
const CURSOR_OUTLINE: u32 = 0xFF00_0000;
const CURSOR_FILL: u32 = 0xFFFF_FFFF;
const CURSOR_TRANSPARENT: u32 = 0x0000_0000;

/// Events emitted by the cursor overlay, with absolute surface-relative
/// coordinates and the surface size needed to map them into Android space.
#[derive(Debug, Clone, Copy)]
pub enum CursorEvent {
    /// Pointer moved to `(x, y)` inside a surface of the given size.
    Motion {
        x: f64,
        y: f64,
        surface_width: u32,
        surface_height: u32,
    },
    /// Left mouse button pressed or released at `(x, y)`.
    LeftButton {
        pressed: bool,
        x: f64,
        y: f64,
        surface_width: u32,
        surface_height: u32,
    },
}

/// Commands sent from the session thread to the overlay thread.
enum CursorOverlayCommand {
    /// Move or hide the session-drawn cursor sprite. The virtual cursor is
    /// rendered by the overlay itself because Gamescope's nested (xdg)
    /// backend implements neither host cursor control nor pointer delivery
    /// to nested clients ("NO CURSOR IMPL XDG").
    SetCursor {
        visible: bool,
        x: f64,
        y: f64,
    },
    Stop,
}

/// Handle to the cursor overlay worker thread.
pub struct CursorOverlay {
    control: Sender<CursorOverlayCommand>,
    thread: Option<JoinHandle<io::Result<()>>>,
}

impl CursorOverlay {
    /// Spawn the overlay thread, connect to the desktop Wayland display, and
    /// block until the overlay is mapped (or fails).
    ///
    /// `output_name` anchors the overlay to the output with that
    /// `wl_output.name` (the KWin focus relay reports the game window's
    /// output); `None` lets the compositor pick an output.
    pub fn spawn<F>(
        runtime_dir: &Path,
        display: &str,
        output_name: Option<&str>,
        emit: F,
    ) -> io::Result<Self>
    where
        F: Fn(CursorEvent) + Send + 'static,
    {
        let (control_tx, control_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let runtime_dir = runtime_dir.to_path_buf();
        let display = display.to_string();
        let output_name = output_name.map(str::to_owned);
        let emit: Box<dyn Fn(CursorEvent) + Send> = Box::new(emit);

        let thread = thread::Builder::new()
            .name("wroid-cursor-overlay".into())
            .spawn(move || {
                run_overlay(
                    runtime_dir,
                    display,
                    output_name,
                    control_rx,
                    emit,
                    ready_tx,
                )
            })?;

        match ready_rx.recv_timeout(READY_TIMEOUT) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => {
                let _ = control_tx.send(CursorOverlayCommand::Stop);
                let _ = thread.join();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "cursor overlay startup timeout",
                ));
            }
        }

        Ok(Self {
            control: control_tx,
            thread: Some(thread),
        })
    }

    /// Position the session-drawn cursor sprite, or hide it.
    ///
    /// Coordinates are surface-local pixels of the overlay surface. The
    /// sprite is only drawn while visible; hidden state commits a transparent
    /// full-surface frame so the layer surface stays mapped.
    /// Queue a sprite update without waiting for the overlay thread.
    ///
    /// Sprite motion happens on the session's input hot path: blocking here
    /// on an overlay round-trip throttled every mouse batch to the overlay
    /// poll interval and built up tens of seconds of input backlog. The
    /// overlay coalesces to the latest queued position anyway, so delivery
    /// order matters more than confirmation.
    pub fn set_cursor_async(&self, visible: bool, x: f64, y: f64) -> io::Result<()> {
        self.control
            .send(CursorOverlayCommand::SetCursor { visible, x, y })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "overlay thread stopped"))
    }
}

impl Drop for CursorOverlay {
    fn drop(&mut self) {
        let _ = self.control.send(CursorOverlayCommand::Stop);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

/// Mutable state shared across the overlay's `Dispatch` implementations.
struct OverlayState {
    configured_size: Option<(u32, u32)>,
    compositor: Option<wl_compositor::WlCompositor>,
    seat: Option<wl_seat::WlSeat>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    surface: Option<wl_surface::WlSurface>,
    layer_surface: Option<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>,
    pointer: Option<wl_pointer::WlPointer>,
    /// Bound outputs with their `wl_output.name`, used to anchor the layer
    /// surface to the game window's output.
    outputs: Vec<(wl_output::WlOutput, Option<String>)>,
    /// Full-surface sprite buffers; the arrow is blitted into them at the
    /// requested position because `wl_surface.attach` forbids nonzero
    /// offsets at version >= 5, so the sprite cannot move via attach.
    sprite_buffers: [Option<wl_buffer::WlBuffer>; SPRITE_BUFFER_COUNT],
    /// Surface size the sprite buffers were created for; drawing is skipped
    /// when the compositor reconfigures a different size.
    sprite_buffer_size: Option<(i32, i32)>,
    sprite_stride: i32,
    /// Arrow tip currently blitted into each sprite buffer.
    sprite_drawn: [Option<(i32, i32)>; SPRITE_BUFFER_COUNT],
    sprite_slots: SpriteBufferSlots,
    /// Last cursor image submitted to the compositor. `None` means no sprite
    /// buffer has been committed yet; `Some(None)` is a committed transparent
    /// frame.
    committed_cursor: Option<Option<(i32, i32)>>,
    sprite_pixels: Option<SpritePixels>,
    pending_cursor_visible: bool,
    pending_cursor_x: i32,
    pending_cursor_y: i32,
    /// Kept alive so the compositor can map the shared-memory pool.
    _cursor_memfd: Option<File>,
    last_x: f64,
    last_y: f64,
    left_pressed: bool,
    emit: Box<dyn Fn(CursorEvent) + Send>,
    ready: Option<Sender<io::Result<()>>>,
    closed: bool,
}

/// Shared-memory mapping of the sprite pool, writable from this thread.
struct SpritePixels {
    pointer: *mut u8,
    length: usize,
}

/// Tracks which shared-memory buffers the compositor still owns.
///
/// Wayland clients may only mutate a buffer after `wl_buffer.release`. Cursor
/// motion can arrive faster than the compositor presents frames, so the latest
/// requested position is kept in `OverlayState` until a slot becomes free.
#[derive(Default)]
struct SpriteBufferSlots {
    busy: [bool; SPRITE_BUFFER_COUNT],
    next: usize,
}

impl SpriteBufferSlots {
    fn acquire(&mut self) -> Option<usize> {
        for offset in 0..self.busy.len() {
            let index = (self.next + offset) % self.busy.len();
            if !self.busy[index] {
                self.busy[index] = true;
                self.next = (index + 1) % self.busy.len();
                return Some(index);
            }
        }
        None
    }

    fn release(&mut self, index: usize) {
        if let Some(busy) = self.busy.get_mut(index) {
            *busy = false;
        }
    }
}

impl SpritePixels {
    fn map(memfd: &File, length: usize) -> io::Result<Self> {
        let pointer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                memfd.as_raw_fd(),
                0,
            )
        };
        if pointer == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            pointer: pointer.cast::<u8>(),
            length,
        })
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.pointer, self.length) }
    }
}

impl Drop for SpritePixels {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.pointer.cast::<libc::c_void>(), self.length);
        }
    }
}

/// Overlay worker entry point: connect, set up the layer surface, then pump
/// events until stopped or the compositor goes away.
fn run_overlay(
    runtime_dir: PathBuf,
    display: String,
    output_name: Option<String>,
    commands: Receiver<CursorOverlayCommand>,
    emit: Box<dyn Fn(CursorEvent) + Send>,
    ready: Sender<io::Result<()>>,
) -> io::Result<()> {
    let socket_path = runtime_dir.join(&display);
    let stream = UnixStream::connect(&socket_path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("connect desktop display {socket_path:?}: {e}"),
        )
    })?;
    let connection = Connection::from_socket(stream)
        .map_err(|e| io::Error::other(format!("wayland handshake: {e}")))?;

    let mut event_queue: EventQueue<OverlayState> = connection.new_event_queue();
    let mut state = OverlayState {
        configured_size: None,
        compositor: None,
        seat: None,
        shm: None,
        layer_shell: None,
        surface: None,
        layer_surface: None,
        pointer: None,
        outputs: Vec::new(),
        sprite_buffers: std::array::from_fn(|_| None),
        sprite_buffer_size: None,
        sprite_stride: 0,
        sprite_drawn: [None; SPRITE_BUFFER_COUNT],
        sprite_slots: SpriteBufferSlots::default(),
        committed_cursor: None,
        sprite_pixels: None,
        pending_cursor_visible: false,
        pending_cursor_x: 0,
        pending_cursor_y: 0,
        _cursor_memfd: None,
        last_x: 0.0,
        last_y: 0.0,
        left_pressed: false,
        emit,
        ready: Some(ready),
        closed: false,
    };

    // Advertise globals.
    let display_handle = connection.display();
    let _registry = display_handle.get_registry(&event_queue.handle(), ());
    event_queue
        .roundtrip(&mut state)
        .map_err(|e| io::Error::other(format!("registry roundtrip: {e}")))?;

    if state.compositor.is_none()
        || state.layer_shell.is_none()
        || state.shm.is_none()
        || state.seat.is_none()
    {
        let err = io::Error::new(
            io::ErrorKind::NotFound,
            "missing required Wayland globals (wl_compositor / zwlr_layer_shell_v1 / wl_shm / wl_seat)",
        );
        if let Some(r) = state.ready.take() {
            let _ = r.send(Err(err));
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "missing required Wayland globals",
        ));
    }

    // Create the fullscreen overlay surface, anchored to the game window's
    // output when the session could name one. Output name events arrive only
    // after their bind, so dispatch one more roundtrip first.
    event_queue
        .roundtrip(&mut state)
        .map_err(|e| io::Error::other(format!("output roundtrip: {e}")))?;
    let anchored_output = output_name.as_deref().and_then(|wanted| {
        state
            .outputs
            .iter()
            .find(|(_, name)| name.as_deref() == Some(wanted))
            .map(|(output, _)| output.clone())
    });
    if output_name.is_some() && anchored_output.is_none() {
        eprintln!(
            "Cursor overlay: output {output_name:?} not found among the desktop outputs; letting \
             the compositor pick one."
        );
    }
    let compositor = state
        .compositor
        .clone()
        .expect("compositor global verified above");
    let layer_shell = state
        .layer_shell
        .clone()
        .expect("layer_shell global verified above");
    let surface = compositor.create_surface(&event_queue.handle(), ());
    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        anchored_output.as_ref(),
        zwlr_layer_shell_v1::Layer::Overlay,
        "wroid-cursor-overlay".to_string(),
        &event_queue.handle(),
        (),
    );
    layer_surface.set_size(0, 0);
    // Cover the whole output, ignoring panels' exclusive zones, so the sprite
    // coordinate space matches the fullscreen game pixel-for-pixel.
    layer_surface.set_exclusive_zone(-1);
    layer_surface.set_anchor(
        zwlr_layer_surface_v1::Anchor::Top
            | zwlr_layer_surface_v1::Anchor::Bottom
            | zwlr_layer_surface_v1::Anchor::Left
            | zwlr_layer_surface_v1::Anchor::Right,
    );
    layer_surface.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);

    surface.commit();
    state.surface = Some(surface);
    state.layer_surface = Some(layer_surface);

    // Wait for the compositor to configure the surface.
    for _ in 0..10 {
        event_queue
            .roundtrip(&mut state)
            .map_err(|e| io::Error::other(format!("configure roundtrip: {e}")))?;
        if state.configured_size.is_some() {
            break;
        }
    }
    if state.configured_size.is_none() {
        if let Some(r) = state.ready.take() {
            let _ = r.send(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "layer surface never configured",
            )));
        }
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "layer surface never configured",
        ));
    }

    // Map the surface with the first (still fully transparent) sprite buffer
    // and prepare the sprite buffers for the session-drawn virtual cursor.
    create_sprite_buffers(&mut state, &event_queue.handle())?;
    let (surface_width, surface_height) = state
        .sprite_buffer_size
        .expect("sprite buffers require a configured size");
    let initial_index = state
        .sprite_slots
        .acquire()
        .expect("new sprite pool has a free buffer");
    if let Some(surface) = state.surface.as_ref() {
        surface.attach(state.sprite_buffers[initial_index].as_ref(), 0, 0);
        surface.damage(0, 0, surface_width, surface_height);
        surface.commit();
        state.committed_cursor = Some(None);
    }
    // Stay input-transparent for the whole session lifetime.
    apply_input_region(&mut state, &event_queue.handle());

    if let Some(r) = state.ready.take() {
        let _ = r.send(Ok(()));
    }

    event_loop(&mut event_queue, &mut state, &commands)
}

/// Create an anonymous memfd of the given size.
fn create_memfd(name: &[u8], size: usize) -> io::Result<File> {
    let fd = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            name.as_ptr() as *const libc::c_char,
            libc::MFD_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd as i32) };
    file.set_len(size as u64)?;
    Ok(file)
}

/// Build the cursor sprite buffers: three full-surface ARGB8888 copies sharing
/// one shm pool. They start fully transparent (fresh memfd pages are
/// zero-filled) and the arrow is blitted into whichever copy is committed
/// next. The third copy lets the client submit a new position while Gamescope
/// still retains the current and previous frames.
fn create_sprite_buffers(
    state: &mut OverlayState,
    qh: &QueueHandle<OverlayState>,
) -> io::Result<()> {
    let Some(shm) = state.shm.clone() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "wl_shm not available",
        ));
    };
    let Some((width, height)) = state.configured_size.and_then(|(width, height)| {
        Some((i32::try_from(width).ok()?, i32::try_from(height).ok()?))
    }) else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "layer surface was not configured yet",
        ));
    };

    let stride = width
        .checked_mul(4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "stride overflow"))?;
    let frame = stride
        .checked_mul(height)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "buffer size overflow"))?;
    let pool_size = frame
        .checked_mul(SPRITE_BUFFER_COUNT as i32)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "pool size overflow"))?;

    let memfd = create_memfd(b"wroid-cursor-sprite\0", pool_size as usize)?;
    let pixels = SpritePixels::map(&memfd, pool_size as usize)?;
    let pool = shm.create_pool(memfd.as_fd(), pool_size, qh, ());
    state.sprite_buffers = std::array::from_fn(|index| {
        Some(pool.create_buffer(
            frame * index as i32,
            width,
            height,
            stride,
            wl_shm::Format::Argb8888,
            qh,
            index,
        ))
    });
    state.sprite_buffer_size = Some((width, height));
    state.sprite_stride = stride;
    state.sprite_drawn = [None; SPRITE_BUFFER_COUNT];
    state.sprite_slots = SpriteBufferSlots::default();
    state.committed_cursor = None;
    state.sprite_pixels = Some(pixels);
    state._cursor_memfd = Some(memfd);
    Ok(())
}

/// Draw (or hide) the virtual cursor sprite on the main overlay surface.
///
/// The arrow is blitted into the next full-surface buffer at the requested
/// position and committed with attach offset `(0, 0)`; `wl_surface.attach`
/// has required zero offsets since version 5, so the sprite moves inside the
/// buffer, not via the attach offset. Alternating buffers keeps every commit
/// distinct for Gamescope's dedupe.
fn draw_cursor(state: &mut OverlayState) {
    let Some(surface) = state.surface.clone() else {
        return;
    };
    let Some((width, height)) = state.sprite_buffer_size else {
        return;
    };
    let matches_configured = match state.configured_size {
        Some((configured_width, configured_height)) => {
            i32::try_from(configured_width) == Ok(width)
                && i32::try_from(configured_height) == Ok(height)
        }
        None => false,
    };
    if !matches_configured {
        // The compositor reconfigured a different size; the pool no longer
        // matches the surface, so skip drawing rather than risk a protocol
        // error.
        return;
    }

    let new_x = state
        .pending_cursor_x
        .clamp(0, width.saturating_sub(CURSOR_WIDTH));
    let new_y = state
        .pending_cursor_y
        .clamp(0, height.saturating_sub(CURSOR_HEIGHT));
    let drawn = state.pending_cursor_visible.then_some((new_x, new_y));

    if state.committed_cursor == Some(drawn) {
        return;
    }
    // KWin (and other compositors with partial texture updates) repaint the
    // surface texture only inside the committed damage region. The sprite
    // previously visible on the surface lives in whichever buffer was
    // committed last — not necessarily the one about to be reused — so both
    // that position and this slot's own stale arrow must be damaged or the
    // old sprite ghosts on screen forever.
    let committed_rect = state.committed_cursor.unwrap_or(None);
    let Some(index) = state.sprite_slots.acquire() else {
        // All buffers are still in use. The newest requested cursor state
        // remains pending and will be drawn when a release event arrives.
        return;
    };
    let old_rect = state.sprite_drawn[index];

    if let Some(pixels) = state.sprite_pixels.as_mut() {
        paint_sprite_frame(
            pixels.as_mut_slice(),
            state.sprite_stride,
            height,
            index,
            old_rect,
            drawn,
        );
    }
    state.sprite_drawn[index] = drawn;

    if let Some(buffer) = state.sprite_buffers[index].as_ref() {
        surface.attach(Some(buffer), 0, 0);
        for rect in [committed_rect, old_rect, drawn].into_iter().flatten() {
            let (x, y) = rect;
            surface.damage(x, y, CURSOR_WIDTH, CURSOR_HEIGHT);
        }
        surface.commit();
        state.committed_cursor = Some(drawn);
    } else {
        state.sprite_slots.release(index);
    }
}

/// Update one full-surface frame in the shared sprite-buffer pool.
fn paint_sprite_frame(
    bytes: &mut [u8],
    stride: i32,
    height: i32,
    index: usize,
    old_rect: Option<(i32, i32)>,
    drawn: Option<(i32, i32)>,
) {
    let Some(frame_len) = stride
        .checked_mul(height)
        .and_then(|length| usize::try_from(length).ok())
    else {
        return;
    };
    let Some(start) = frame_len.checked_mul(index) else {
        return;
    };
    let Some(end) = start.checked_add(frame_len) else {
        return;
    };
    let Some(frame) = bytes.get_mut(start..end) else {
        return;
    };

    clear_sprite_rect(frame, stride, old_rect);
    if let Some((x, y)) = drawn {
        blit_arrow(frame, stride, x, y);
    }
}

/// Erase the arrow at `rect` from the sprite pool bytes.
fn clear_sprite_rect(bytes: &mut [u8], stride: i32, rect: Option<(i32, i32)>) {
    let Some((x, y)) = rect else {
        return;
    };
    for row in 0..CURSOR_HEIGHT {
        let start = ((y + row) * stride + x * 4) as usize;
        let end = start + CURSOR_WIDTH as usize * 4;
        if end <= bytes.len() {
            bytes[start..end].fill(0);
        }
    }
}

/// Blit the arrow bitmap with its tip at `(x, y)` into the sprite pool bytes.
fn blit_arrow(bytes: &mut [u8], stride: i32, x: i32, y: i32) {
    for (row_index, row) in CURSOR_ROWS.iter().enumerate() {
        for column in 0..CURSOR_WIDTH {
            let cell = row
                .chars()
                .nth(column as usize)
                .filter(|c| *c == 'X' || *c == 'o')
                .map(|c| {
                    if c == 'X' {
                        CURSOR_OUTLINE
                    } else {
                        CURSOR_FILL
                    }
                })
                .unwrap_or(CURSOR_TRANSPARENT);
            let start = ((y + row_index as i32) * stride + (x + column) * 4) as usize;
            let end = start + 4;
            if end <= bytes.len() {
                bytes[start..end].copy_from_slice(&cell.to_ne_bytes());
            }
        }
    }
}

/// Install the empty input region that keeps the overlay input-transparent.
///
/// The session grabs the physical mouse itself; the overlay is a pure
/// renderer and must never intercept desktop input.
fn apply_input_region(state: &mut OverlayState, qh: &QueueHandle<OverlayState>) {
    let Some(surface) = state.surface.as_ref() else {
        return;
    };
    let Some(compositor) = state.compositor.as_ref() else {
        return;
    };
    let region = compositor.create_region(qh, ());
    surface.set_input_region(Some(&region));
    region.destroy();
    surface.commit();
}

/// Main event pump: interleave command handling with Wayland dispatch.
fn event_loop(
    event_queue: &mut EventQueue<OverlayState>,
    state: &mut OverlayState,
    commands: &Receiver<CursorOverlayCommand>,
) -> io::Result<()> {
    loop {
        if state.closed {
            return Ok(());
        }

        // Drain pending control commands.
        loop {
            match commands.try_recv() {
                Ok(CursorOverlayCommand::SetCursor { visible, x, y }) => {
                    state.pending_cursor_visible = visible;
                    state.pending_cursor_x = x as i32;
                    state.pending_cursor_y = y as i32;
                    draw_cursor(state);
                }
                Ok(CursorOverlayCommand::Stop) => return Ok(()),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }

        // Dispatch already-buffered Wayland events.
        event_queue
            .dispatch_pending(state)
            .map_err(|e| io::Error::other(format!("dispatch: {e}")))?;

        // Flush outgoing requests.
        event_queue
            .flush()
            .map_err(|e| io::Error::other(format!("flush: {e}")))?;

        // Prepare to read more events; if more dispatching is needed first,
        // prepare_read returns None and we loop.
        let Some(guard) = event_queue.prepare_read() else {
            continue;
        };

        // Poll the connection fd with a timeout so we periodically re-check
        // commands even when no Wayland traffic arrives.
        let fd = guard.connection_fd();
        let mut pfd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, POLL_TIMEOUT_MS) };
        if ret > 0 && (pfd.revents & libc::POLLIN) != 0 {
            guard
                .read()
                .map_err(|e| io::Error::other(format!("read: {e}")))?;
        } else {
            drop(guard);
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatch implementations
// ---------------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, ()> for OverlayState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    let version = version.min(5);
                    state.compositor = Some(registry.bind(name, version, qh, ()));
                }
                "wl_output" => {
                    // Version 4 carries the `name` event used to anchor the
                    // overlay to a specific output.
                    let version = version.min(4);
                    state
                        .outputs
                        .push((registry.bind(name, version, qh, ()), None));
                }
                "wl_seat" => {
                    let version = version.min(9);
                    state.seat = Some(registry.bind(name, version, qh, ()));
                }
                "wl_shm" => {
                    let version = version.min(2);
                    state.shm = Some(registry.bind(name, version, qh, ()));
                }
                "zwlr_layer_shell_v1" => {
                    let version = version.min(4);
                    state.layer_shell = Some(registry.bind(name, version, qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for OverlayState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_compositor::WlCompositor,
        _event: wl_compositor::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for OverlayState {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            let has_pointer = matches!(
                capabilities,
                wayland_client::WEnum::Value(caps) if caps.contains(wl_seat::Capability::Pointer)
            );
            if has_pointer {
                if state.pointer.is_none() {
                    state.pointer = Some(seat.get_pointer(qh, ()));
                }
            } else if let Some(pointer) = state.pointer.take() {
                pointer.release();
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for OverlayState {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some((_, slot)) = state.outputs.iter_mut().find(|(bound, _)| bound == output) {
                *slot = Some(name);
            }
        }
    }
}

impl Dispatch<wl_shm::WlShm, ()> for OverlayState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm::WlShm,
        _event: wl_shm::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for OverlayState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm_pool::WlShmPool,
        _event: wl_shm_pool::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, usize> for OverlayState {
    fn event(
        state: &mut Self,
        _proxy: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        index: &usize,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            state.sprite_slots.release(*index);
            draw_cursor(state);
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for OverlayState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_surface::WlSurface,
        _event: wl_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_region::WlRegion, ()> for OverlayState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_region::WlRegion,
        _event: wl_region::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for OverlayState {
    fn event(
        state: &mut Self,
        _proxy: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            // Pointer events are only possible on compositors that actually
            // deliver host pointers to nested clients; Gamescope's nested
            // backend does not, and the session-drawn virtual cursor is the
            // primary path. These remain as a secondary source.
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            } => {
                state.last_x = surface_x;
                state.last_y = surface_y;
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.last_x = surface_x;
                state.last_y = surface_y;
                if let Some((w, h)) = state.configured_size {
                    (state.emit)(CursorEvent::Motion {
                        x: surface_x,
                        y: surface_y,
                        surface_width: w,
                        surface_height: h,
                    });
                }
            }
            wl_pointer::Event::Button {
                button,
                state: button_state,
                ..
            } => {
                if button == BTN_LEFT {
                    let pressed = button_state
                        == wayland_client::WEnum::Value(wl_pointer::ButtonState::Pressed);
                    state.left_pressed = pressed;
                    if let Some((w, h)) = state.configured_size {
                        (state.emit)(CursorEvent::LeftButton {
                            pressed,
                            x: state.last_x,
                            y: state.last_y,
                            surface_width: w,
                            surface_height: h,
                        });
                    }
                }
            }
            wl_pointer::Event::Leave { .. } => {
                // Release any held contact so we never leave a stuck touch.
                if state.left_pressed {
                    state.left_pressed = false;
                    if let Some((w, h)) = state.configured_size {
                        (state.emit)(CursorEvent::LeftButton {
                            pressed: false,
                            x: state.last_x,
                            y: state.last_y,
                            surface_width: w,
                            surface_height: h,
                        });
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_layer_shell_v1::ZwlrLayerShellV1, ()> for OverlayState {
    fn event(
        _state: &mut Self,
        _proxy: &zwlr_layer_shell_v1::ZwlrLayerShellV1,
        _event: zwlr_layer_shell_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for OverlayState {
    fn event(
        state: &mut Self,
        _proxy: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                if let Some(layer_surface) = state.layer_surface.as_ref() {
                    layer_surface.ack_configure(serial);
                }
                state.configured_size = Some((width, height));
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.closed = true;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrow_blits_and_clears_within_bounds() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let mut bytes = vec![0_u8; (stride * height) as usize];
        blit_arrow(&mut bytes, stride, 10, 20);
        let pixel = |bytes: &[u8], x: i32, y: i32| {
            let start = (y * stride + x * 4) as usize;
            u32::from_ne_bytes(bytes[start..start + 4].try_into().unwrap())
        };
        assert_eq!(pixel(&bytes, 10, 20), CURSOR_OUTLINE);
        assert_eq!(pixel(&bytes, 11, 22), CURSOR_FILL);
        assert_eq!(pixel(&bytes, 30, 30), CURSOR_TRANSPARENT);

        clear_sprite_rect(&mut bytes, stride, Some((10, 20)));
        assert_eq!(pixel(&bytes, 10, 20), CURSOR_TRANSPARENT);
        assert_eq!(pixel(&bytes, 11, 22), CURSOR_TRANSPARENT);
    }

    #[test]
    fn sprite_paint_targets_the_selected_shm_frame() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let frame_len = (stride * height) as usize;
        let mut bytes = vec![0_u8; frame_len * SPRITE_BUFFER_COUNT];

        paint_sprite_frame(&mut bytes, stride, height, 2, None, Some((10, 20)));

        assert!(bytes[..frame_len * 2].iter().all(|byte| *byte == 0));
        let pixel_start = frame_len * 2 + (20 * stride + 10 * 4) as usize;
        assert_eq!(
            u32::from_ne_bytes(bytes[pixel_start..pixel_start + 4].try_into().unwrap()),
            CURSOR_OUTLINE
        );
    }

    #[test]
    fn sprite_buffers_are_not_reused_before_release() {
        let mut slots = SpriteBufferSlots::default();

        // Gamescope may retain the previous frame while presenting the
        // current one. The startup transparent frame and the first visible
        // frame must therefore leave a third writable slot for the first
        // actual mouse movement.
        assert_eq!(slots.acquire(), Some(0));
        assert_eq!(slots.acquire(), Some(1));
        assert_eq!(slots.acquire(), Some(2));
        assert_eq!(slots.acquire(), None);

        slots.release(0);
        assert_eq!(slots.acquire(), Some(0));
    }

    #[test]
    #[ignore = "requires a live desktop Wayland session; verify with screenshots"]
    fn live_desktop_overlay_presents_movable_sprite() {
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .expect("XDG_RUNTIME_DIR is set in a desktop session");
        let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".to_owned());
        let (events_tx, events_rx) = mpsc::channel();
        let overlay = CursorOverlay::spawn(&runtime_dir, &display, None, move |event| {
            let _ = events_tx.send(event);
        })
        .expect("overlay maps on the desktop compositor");

        // Hold each position long enough for out-of-band screenshot checks:
        // the arrow must be composited at the requested surface-local
        // coordinates and must move between phases.
        for (x, y) in [(960.0, 540.0), (400.0, 300.0)] {
            overlay
                .set_cursor_async(true, x, y)
                .expect("cursor command");
            thread::sleep(Duration::from_secs(4));
        }
        overlay
            .set_cursor_async(false, 0.0, 0.0)
            .expect("hide command");
        drop(overlay);

        // Compositor pointer delivery is a secondary source; an empty input
        // region must keep it silent so the overlay never steals input.
        assert!(events_rx.try_recv().is_err());
    }
}
